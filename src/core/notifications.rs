//! GTK-free Notifications-page domain logic (task 6.7; architecture §3, §6; R4.2,
//! R4.4, R5.2, R5.6, R6.2).
//!
//! # What this module is
//!
//! The Notifications page edits three things that split cleanly into two very different
//! mechanisms, and this module supplies the headless half of each:
//!
//! - **Position and auto-dismiss timeout** are ordinary file-backed settings
//!   ([`SettingId::NotificationPosition`] / [`SettingId::NotificationTimeout`]) staged in
//!   the shared [`SettingsStore`](crate::core::store) and rendered by the declarative row
//!   framework (task 5.2), exactly like the Input page. This module provides the
//!   **store-`SettingId` → `swaync/config.json` write glue**: [`render_swaync_config`]
//!   applies the store's dirty Notifications settings to the JSON through the task-3.4
//!   adapter, producing one [`FileWrite`] the shared Apply pipeline runs (mirroring
//!   [`crate::core::input::render_input_conf`]). The pipeline then reloads swaync with
//!   `swaync-client -rs` (task 4.4). Because `config.json` is store-loaded (baselined by
//!   the store, whose tracker `apply::run` conflict-checks and which `commit_apply`
//!   re-baselines), conflict-safety (R5.6) needs no bespoke tracker here — like Input.
//!
//! - **Do Not Disturb is *not* a persisted config setting.** swaync's DND is **runtime
//!   daemon state**, toggled through `swaync-client`; the on-disk `config.json` has no
//!   top-level `dnd` boolean (only a `widgets` entry and a `widget-config.dnd` label —
//!   see [`crate::parsers::swaync`]). Writing a `dnd` key would just append a dead key
//!   swaync ignores. So DND is modelled here as a **runtime-only control** (R5.2), exactly
//!   like the Sound page's volume/mute: it reads the live state with `swaync-client
//!   --get-dnd` ([`dnd_state`]) and sets it immediately with `swaync-client --dnd-on` /
//!   `--dnd-off` ([`set_dnd`]). It is never staged, never dirty, holds no [`SettingId`],
//!   and writes no config key — the `render_swaync_config` write glue only ever touches
//!   position and timeout.
//!
//! It lives in `core/` because every piece is pure domain logic over bytes or the
//! command seam — no GTK — so it is unit-tested headlessly (R6.2); the layering guard in
//! `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import here.
//!
//! # Position is one setting on-disk in two keys
//!
//! The store carries the notification anchor as a single [`Value::Enum`] token combining
//! the two on-disk halves — `positionY` (`top`/`bottom`) and `positionX`
//! (`left`/`center`/`right`) — as `"<positionY>-<positionX>"` (e.g. `top-right`). The
//! startup loader composes that token when reading (see [`crate::ui::startup`]); this
//! module performs the reverse **decompose** back into the two string keys when writing
//! ([`decompose_position`]), so a position edit changes exactly `positionY` and
//! `positionX`.

use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::core::apply::FileWrite;
use crate::core::model::{SettingId, Value};
use crate::core::reload::BackingFile;
use crate::parsers::swaync::{ParseError, SwayncConfigFile};
use crate::system::command::{Command, CommandRunner};

/// The `config.json` key holding the vertical anchor half of the notification position
/// (`top`/`bottom`).
const KEY_POSITION_Y: &str = "positionY";
/// The `config.json` key holding the horizontal anchor half (`left`/`center`/`right`).
const KEY_POSITION_X: &str = "positionX";
/// The `config.json` key holding the default auto-dismiss timeout, in whole seconds.
const KEY_TIMEOUT: &str = "timeout";

/// The `swaync-client` executable that drives every runtime DND action.
const SWAYNC_CLIENT: &str = "swaync-client";
/// `swaync-client` flag that prints the current do-not-disturb state (`true`/`false`),
/// confirmed against `swaync-client(1)`.
const FLAG_GET_DND: &str = "--get-dnd";
/// `swaync-client` flag that turns do-not-disturb on and prints the new state.
const FLAG_DND_ON: &str = "--dnd-on";
/// `swaync-client` flag that turns do-not-disturb off and prints the new state.
const FLAG_DND_OFF: &str = "--dnd-off";

/// The complete new `config.json` bytes plus the changed-key labels, produced by applying
/// the store's dirty Notifications edits to the current file (task 6.7).
///
/// Returned by [`render_swaync_config`] and wrapped in a [`FileWrite`] by
/// [`NotificationsModel::swaync_config_write`]. The `changed_keys` are the JSON keys the
/// edit touched (e.g. `positionY`, `timeout`), used only for the apply-level log line
/// (R7.3), never the file contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwayncConfEdit {
    /// The complete new file contents — canonical 2-space pretty JSON with a trailing
    /// newline, with only the edited value spans changed and key order preserved (§3.4).
    pub contents: Vec<u8>,
    /// The JSON keys this edit changed, for logging.
    pub changed_keys: Vec<String>,
}

/// Why [`render_swaync_config`] could not produce the new bytes.
///
/// Unlike the line-oriented parsers, the swaync JSON adapter is all-or-nothing: malformed
/// JSON has no partial representation, so a re-read that no longer parses is surfaced as
/// an error rather than a panic. In practice this is near-unreachable — the file parsed
/// successfully at startup — but it is treated as a failure so a corrupted file aborts
/// the apply instead of the store committing an unwritten value (R8.3).
#[derive(Debug)]
pub enum SwayncRenderError {
    /// The current `config.json` bytes are no longer valid swaync JSON.
    Parse(ParseError),
}

impl fmt::Display for SwayncRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwayncRenderError::Parse(error) => {
                write!(f, "swaync config.json could not be parsed: {error}")
            }
        }
    }
}

impl std::error::Error for SwayncRenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SwayncRenderError::Parse(error) => Some(error),
        }
    }
}

/// Why [`NotificationsModel::swaync_config_write`] could not produce a write despite there
/// being dirty Notifications settings to apply (task 6.7, the same abort-not-skip
/// contract as the Input page's `InputWriteError`).
///
/// This is distinct from "nothing was dirty" (a plain `Ok(None)`): when the user *has*
/// pending Notifications edits but the write cannot be rendered, the Apply must **abort**
/// rather than skip the write and let the store commit the staged values against an
/// unchanged file — that would desync the store from disk. Both cases are near-unreachable
/// in practice (`config.json` is readable and was parseable at load), but treating them as
/// failures keeps the store and the file in agreement (R8.3).
#[derive(Debug)]
pub enum SwayncWriteError {
    /// `config.json` could not be read to render the edits.
    Read(io::Error),
    /// The JSON could no longer be parsed to apply the edits.
    Render(SwayncRenderError),
}

impl fmt::Display for SwayncWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwayncWriteError::Read(error) => {
                write!(f, "swaync config.json could not be read: {error}")
            }
            SwayncWriteError::Render(error) => {
                write!(
                    f,
                    "the swaync config.json edit could not be applied: {error}"
                )
            }
        }
    }
}

impl std::error::Error for SwayncWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SwayncWriteError::Read(error) => Some(error),
            SwayncWriteError::Render(error) => Some(error),
        }
    }
}

/// Splits the store's combined position token (`"<positionY>-<positionX>"`) back into its
/// two on-disk halves, or `None` for a token with no separator.
///
/// The startup loader composes the token as `format!("{positionY}-{positionX}")`, and the
/// vertical half (`top`/`bottom`) never contains a hyphen, so splitting on the **first**
/// hyphen unambiguously recovers `(positionY, positionX)` (e.g. `top-right` →
/// `("top", "right")`, `bottom-center` → `("bottom", "center")`). A malformed token
/// without a hyphen — which the fixed drop-down never produces — yields `None` and is
/// skipped by [`render_swaync_config`] rather than written as a bad value.
fn decompose_position(token: &str) -> Option<(&str, &str)> {
    token.split_once('-')
}

/// Applies the store's dirty Notifications `edits` to the current `config.json` `bytes`,
/// returning the complete new bytes (task 6.7; R5.3 item 1).
///
/// This is the pure store-`SettingId` → `config.json` glue, mirroring
/// [`crate::core::input::render_input_conf`]. It parses the current JSON through the
/// task-3.4 adapter (which preserves key order), then for each `(id, value)` sets **only**
/// that setting's key(s) in place — [`SettingId::NotificationPosition`] decomposed back
/// into `positionY`+`positionX`, [`SettingId::NotificationTimeout`] into the integer
/// `timeout` key — leaving every other key, and the key order, byte-identical (§3.4).
///
/// Do Not Disturb is deliberately **not** handled here: it is runtime daemon state
/// ([`set_dnd`]), not a config key, so this glue never writes a `dnd` key. A setting that
/// is not one of the two Notifications keys, or whose value kind does not match (a guard
/// the store already rejects on stage), is skipped rather than written as a bad value.
///
/// Returns [`SwayncRenderError`] only if the current bytes are no longer valid swaync
/// JSON, leaving the caller to abort the apply rather than emit a corrupt file.
pub fn render_swaync_config(
    bytes: &[u8],
    edits: &[(SettingId, Value)],
) -> Result<SwayncConfEdit, SwayncRenderError> {
    let text = String::from_utf8_lossy(bytes);
    let mut config = SwayncConfigFile::parse(&text).map_err(SwayncRenderError::Parse)?;

    let mut changed_keys = Vec::new();
    for (id, value) in edits {
        match id {
            SettingId::NotificationPosition => {
                // The store token is `"<positionY>-<positionX>"`; write each half to its
                // own key. A token that does not decompose (never produced by the fixed
                // drop-down) is skipped rather than written malformed.
                let Some((position_y, position_x)) = value.as_enum().and_then(decompose_position)
                else {
                    continue;
                };
                config.set_string(KEY_POSITION_Y, position_y);
                config.set_string(KEY_POSITION_X, position_x);
                changed_keys.push(KEY_POSITION_Y.to_string());
                changed_keys.push(KEY_POSITION_X.to_string());
            }
            SettingId::NotificationTimeout => {
                let Some(seconds) = value.as_integer() else {
                    continue;
                };
                config.set_integer(KEY_TIMEOUT, seconds);
                changed_keys.push(KEY_TIMEOUT.to_string());
            }
            // Not a Notifications-backed setting (a foreign id): skip it — it does not
            // belong to this file.
            _ => continue,
        }
    }

    Ok(SwayncConfEdit {
        contents: config.emit().into_bytes(),
        changed_keys,
    })
}

/// The Notifications page's Apply-time write glue for `swaync/config.json` (task 6.7).
///
/// Built once on the startup worker when swaync is present, and held by the window. Like
/// the Input page's [`InputModel`](crate::core::input::InputModel) it is **not** a staging
/// model — the position and timeout are staged in the shared store, so dirty tracking,
/// validation, reset, and commit all flow through the store — and it owns **no**
/// [`FreshnessTracker`](crate::core::freshness::FreshnessTracker): `config.json` is
/// store-loaded, so the store baselines it and the Apply pipeline conflict-checks it. This
/// just turns the store's dirty Notifications settings into the one `config.json`
/// [`FileWrite`] on Apply. (Do Not Disturb needs no state here — it is applied through the
/// free [`set_dnd`]/[`dnd_state`] functions by the UI.)
pub struct NotificationsModel {
    /// The live XDG path of `swaync/config.json` (R8.5), read fresh when rendering a write.
    swaync_config: PathBuf,
}

impl NotificationsModel {
    /// Builds the model, recording the `config.json` path (the production entry point,
    /// called from the startup worker — architecture §8).
    pub fn load(swaync_config: PathBuf) -> NotificationsModel {
        NotificationsModel { swaync_config }
    }

    /// Renders the store's dirty Notifications settings into a `config.json` [`FileWrite`]
    /// (task 6.7).
    ///
    /// `dirty` is the store's dirty Notifications settings (from
    /// [`SettingsStore::dirty_in_category`](crate::core::store::SettingsStore::dirty_in_category)).
    /// It reads the current file bytes and applies the edits through
    /// [`render_swaync_config`], returning a single [`FileWrite`] for the shared Apply
    /// pipeline. Reading fresh each time — rather than caching a parsed copy — is what
    /// keeps it correct across repeated applies and external edits without a bespoke
    /// freshness tracker: the pipeline's conflict check (against the store's baseline)
    /// aborts the apply if the file changed since load, so a fresh read can never clobber
    /// an external edit (the same contract as [`InputModel`](crate::core::input::InputModel)).
    ///
    /// Returns:
    /// - `Ok(None)` when there is nothing to write — `dirty` is empty (the common clean
    ///   case);
    /// - `Ok(Some(write))` with the rendered write;
    /// - `Err(SwayncWriteError)` when there *are* dirty Notifications settings but the
    ///   write cannot be produced (the file is unreadable, or the JSON no longer parses).
    ///   The caller must **abort the Apply** rather than skip the write, since the store
    ///   would otherwise commit the staged values against an unchanged file and desync.
    ///   Both failure modes are near-unreachable in practice.
    pub fn swaync_config_write(
        &self,
        dirty: &[(SettingId, Value)],
    ) -> Result<Option<FileWrite>, SwayncWriteError> {
        if dirty.is_empty() {
            return Ok(None);
        }
        let bytes = std::fs::read(&self.swaync_config).map_err(|error| {
            tracing::error!(
                path = %self.swaync_config.display(),
                %error,
                "could not read swaync config.json to render the Notifications edits; \
                 aborting the apply (R8.3)"
            );
            SwayncWriteError::Read(error)
        })?;
        let edit = render_swaync_config(&bytes, dirty).map_err(|error| {
            tracing::error!(
                path = %self.swaync_config.display(),
                %error,
                "failed to render a swaync config.json edit; aborting the apply (R8.3)"
            );
            SwayncWriteError::Render(error)
        })?;
        // `dirty` is non-empty here (the early return above handles the empty case), and
        // every Notifications setting maps to at least one JSON key with a rendered value,
        // so `changed_keys` is always non-empty at this point — this branch is truly
        // unreachable. It is kept only as a guard against emitting a byte-identical no-op
        // write; unlike the `Read`/`Render` errors above, which correctly abort a genuine
        // runtime failure, reaching here would signal a logic bug, not a failure.
        if edit.changed_keys.is_empty() {
            return Ok(None);
        }
        Ok(Some(FileWrite {
            path: self.swaync_config.clone(),
            contents: edit.contents,
            changed_keys: edit.changed_keys,
            backing: BackingFile::SwayncConfig,
        }))
    }
}

/// Builds the `swaync-client --get-dnd` command that prints the current do-not-disturb
/// state.
fn dnd_query_command() -> Command {
    Command::new(SWAYNC_CLIENT).arg(FLAG_GET_DND)
}

/// Builds the `swaync-client --dnd-on` / `--dnd-off` command that sets do-not-disturb to
/// the requested state (R5.2).
///
/// Setting the state directly (rather than the `--toggle-dnd` flag) is what lets a
/// [`GtkSwitch`](https://docs.gtk.org/gtk4/class.Switch.html)-style control drive DND
/// without a race: the switch knows the desired state, so it asks for exactly that instead
/// of a toggle that could invert an already-matching daemon state.
fn set_dnd_command(enabled: bool) -> Command {
    Command::new(SWAYNC_CLIENT).arg(if enabled { FLAG_DND_ON } else { FLAG_DND_OFF })
}

/// Reads swaync's current do-not-disturb state from the running daemon, or `None` when it
/// cannot be determined (task 6.7, R5.2).
///
/// Runs `swaync-client --get-dnd`, which prints `true`/`false`, through the command seam.
/// A daemon that is not running, a command that fails, or output that does not parse all
/// degrade to `None` (logged at `info`, never an error) — the UI then shows the switch as
/// off, since an unreachable daemon has no DND to report. This never touches a config file
/// or the store: DND is live daemon state, not a persisted setting.
pub fn dnd_state(runner: &dyn CommandRunner) -> Option<bool> {
    let command = dnd_query_command();
    match runner.run(&command) {
        Ok(output) if output.success() => {
            let state = parse_dnd(output.stdout());
            if state.is_none() {
                tracing::info!(%command, "swaync DND state was not parseable; treating as unknown");
            }
            state
        }
        Ok(output) => {
            tracing::info!(
                %command,
                code = ?output.code(),
                "swaync-client --get-dnd failed; DND state unknown"
            );
            None
        }
        Err(error) => {
            tracing::info!(%command, %error, "could not run swaync-client --get-dnd; DND state unknown");
            None
        }
    }
}

/// Parses the `swaync-client --get-dnd` output (`true`/`false`, with a trailing newline)
/// into a boolean, or `None` for anything unrecognised.
fn parse_dnd(stdout: &[u8]) -> Option<bool> {
    match String::from_utf8_lossy(stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Immediately sets swaync's do-not-disturb to `enabled` (R5.2), logging the outcome.
///
/// Runs `swaync-client --dnd-on` / `--dnd-off` through the command seam. This is a
/// runtime-only control: it bypasses staging entirely, touches no config file, and is
/// applied at once — so nothing here is dirty or committed. A failure (e.g. the daemon is
/// not running) is logged at `error` and otherwise ignored — it is non-fatal and leaves
/// the daemon's DND state as it was.
pub fn set_dnd(runner: &dyn CommandRunner, enabled: bool) {
    let command = set_dnd_command(enabled);
    match runner.run(&command) {
        Ok(output) if output.success() => {
            tracing::info!(%command, enabled, "set swaync do-not-disturb (runtime-only, R5.2)");
        }
        Ok(output) => {
            tracing::error!(%command, code = ?output.code(), "swaync do-not-disturb command failed");
        }
        Err(error) => {
            tracing::error!(%command, %error, "could not run swaync do-not-disturb command");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::core::apply::{self, ApplyOutcome, ApplyPlan};
    use crate::core::detect::{Capabilities, Daemon};
    use crate::core::freshness::FreshnessTracker;
    use crate::core::model::Category;
    use crate::core::reload::ReloadParams;
    use crate::core::store::{FileReader, FileValues, SettingsStore};
    use crate::system::command::{Command, CommandOutput, MockCommandRunner};
    use crate::system::signal::MockProcessSignaller;

    /// A realistic `swaync/config.json` fixture in swaync's exact on-disk shape (canonical
    /// two-space pretty JSON with a trailing newline), carrying the two edited keys
    /// (`positionX`/`positionY`, `timeout`) among unrelated keys, a nested array, and the
    /// `dnd` *widget* entry — so an edit's "nothing else changed" and "no `dnd` key added"
    /// guarantees are both exercised. Keys are drawn from the real config (analysis §4).
    const CONFIG_JSON: &str = "\
{
  \"$schema\": \"/etc/xdg/swaync/configSchema.json\",
  \"positionX\": \"right\",
  \"positionY\": \"top\",
  \"control-center-margin-top\": 8,
  \"timeout\": 10,
  \"timeout-low\": 5,
  \"timeout-critical\": 0,
  \"keyboard-shortcuts\": true,
  \"widgets\": [
    \"title\",
    \"dnd\",
    \"notifications\"
  ]
}
";

    /// Renders `edits` into `CONFIG_JSON` and returns the emitted text.
    fn render(edits: &[(SettingId, Value)]) -> String {
        let edit = render_swaync_config(CONFIG_JSON.as_bytes(), edits).expect("edits render");
        String::from_utf8(edit.contents).expect("valid utf-8")
    }

    #[test]
    fn decompose_position_splits_on_the_first_hyphen() {
        // The reverse of the startup loader's compose: the combined token splits back into
        // (positionY, positionX). positionY never contains a hyphen, so the first-hyphen
        // split is unambiguous even for the `center` horizontal value.
        assert_eq!(decompose_position("top-right"), Some(("top", "right")));
        assert_eq!(decompose_position("bottom-left"), Some(("bottom", "left")));
        assert_eq!(
            decompose_position("bottom-center"),
            Some(("bottom", "center"))
        );
        // swaync's vertical `center` half decomposes like top/bottom; the
        // `center-center` "dead centre" splits cleanly on the first hyphen.
        assert_eq!(
            decompose_position("center-right"),
            Some(("center", "right"))
        );
        assert_eq!(
            decompose_position("center-center"),
            Some(("center", "center"))
        );
        // A malformed token (no hyphen) does not decompose.
        assert_eq!(decompose_position("top"), None);
    }

    #[test]
    fn a_center_position_edit_writes_position_y_center_and_the_x_key() {
        // A `center-*` token decomposes to `positionY: center` + the X key — a
        // JSON round-trip that changes exactly those two keys, so a live `center` config
        // both preselects (page.rs) and round-trips (here).
        let edit = render_swaync_config(
            CONFIG_JSON.as_bytes(),
            &[(
                SettingId::NotificationPosition,
                Value::Enum("center-center".to_string()),
            )],
        )
        .expect("center position renders");
        assert_eq!(
            edit.changed_keys,
            vec!["positionY".to_string(), "positionX".to_string()]
        );
        let expected = CONFIG_JSON
            .replace("\"positionX\": \"right\"", "\"positionX\": \"center\"")
            .replace("\"positionY\": \"top\"", "\"positionY\": \"center\"");
        assert_eq!(
            String::from_utf8(edit.contents).expect("valid utf-8"),
            expected
        );
    }

    #[test]
    fn a_position_edit_decomposes_to_position_y_and_position_x() {
        // Accept criterion: a position edit changes exactly `positionY` and `positionX`,
        // decomposed from the store's combined token, and leaves every other key — and the
        // key order — byte-identical (a JSON round-trip with stable key order).
        let edit = render_swaync_config(
            CONFIG_JSON.as_bytes(),
            &[(
                SettingId::NotificationPosition,
                Value::Enum("bottom-left".to_string()),
            )],
        )
        .expect("position edit renders");
        assert_eq!(
            edit.changed_keys,
            vec!["positionY".to_string(), "positionX".to_string()],
            "a position edit touches exactly the two anchor keys"
        );

        // The strongest "nothing else moved" assertion: the whole file equals the fixture
        // with only the two anchor value spans changed, so surrounding keys, the nested
        // array, and the key order are all preserved.
        let expected = CONFIG_JSON
            .replace("\"positionX\": \"right\"", "\"positionX\": \"left\"")
            .replace("\"positionY\": \"top\"", "\"positionY\": \"bottom\"");
        assert_eq!(
            String::from_utf8(edit.contents).expect("valid utf-8"),
            expected,
            "only positionY/positionX change; all other bytes and the key order stay put"
        );
    }

    #[test]
    fn a_timeout_edit_sets_the_integer_key() {
        // Accept criterion: a timeout edit sets the integer `timeout` key only, leaving
        // timeout-low/timeout-critical and every other key (and the order) untouched.
        let edit = render_swaync_config(
            CONFIG_JSON.as_bytes(),
            &[(SettingId::NotificationTimeout, Value::Integer(25))],
        )
        .expect("timeout edit renders");
        assert_eq!(edit.changed_keys, vec!["timeout".to_string()]);

        // `"timeout": 10` is unique (the `: 10` distinguishes it from timeout-low: 5 and
        // timeout-critical: 0), so the single replacement is unambiguous.
        let expected = CONFIG_JSON.replace("\"timeout\": 10", "\"timeout\": 25");
        assert_eq!(
            String::from_utf8(edit.contents).expect("valid utf-8"),
            expected,
            "only the integer timeout changes; timeout-low/timeout-critical are untouched"
        );
    }

    #[test]
    fn a_combined_edit_changes_only_the_edited_keys() {
        // Position and timeout together: exactly positionY, positionX, and timeout change;
        // the key order and all other content are preserved.
        let edited = render(&[
            (
                SettingId::NotificationPosition,
                Value::Enum("bottom-center".to_string()),
            ),
            (SettingId::NotificationTimeout, Value::Integer(3)),
        ]);
        let expected = CONFIG_JSON
            .replace("\"positionX\": \"right\"", "\"positionX\": \"center\"")
            .replace("\"positionY\": \"top\"", "\"positionY\": \"bottom\"")
            .replace("\"timeout\": 10", "\"timeout\": 3");
        assert_eq!(edited, expected);
    }

    #[test]
    fn render_never_introduces_a_dnd_key() {
        // The DND gotcha, enforced: the config write glue only ever touches
        // position/timeout, so editing them never adds a top-level `dnd` boolean. DND is
        // runtime daemon state (`set_dnd`), never a persisted config key.
        let edited = render(&[
            (
                SettingId::NotificationPosition,
                Value::Enum("top-left".to_string()),
            ),
            (SettingId::NotificationTimeout, Value::Integer(7)),
        ]);
        let reparsed = SwayncConfigFile::parse(&edited).expect("re-parse the emitted config");
        assert_eq!(
            reparsed.boolean("dnd"),
            None,
            "editing position/timeout must never add a top-level `dnd` config key"
        );
    }

    #[test]
    fn no_edits_round_trips_the_file_byte_for_byte() {
        // With no dirty settings the render is the identity — the file is already in
        // canonical form, so it round-trips byte-for-byte with no changed keys.
        let edit = render_swaync_config(CONFIG_JSON.as_bytes(), &[]).expect("renders");
        assert_eq!(edit.contents, CONFIG_JSON.as_bytes());
        assert!(edit.changed_keys.is_empty());
    }

    #[test]
    fn a_foreign_setting_is_skipped() {
        // A setting that is not a Notifications key (a caller passing a foreign id) is
        // ignored — it does not belong to this file, so nothing changes.
        let edit = render_swaync_config(
            CONFIG_JSON.as_bytes(),
            &[(SettingId::MouseSensitivity, Value::Float(0.5))],
        )
        .expect("renders");
        assert_eq!(edit.contents, CONFIG_JSON.as_bytes());
        assert!(edit.changed_keys.is_empty());
    }

    #[test]
    fn render_errors_on_malformed_json() {
        // A re-read that no longer parses surfaces as an error so the Apply aborts, rather
        // than panicking or emitting a corrupt file.
        let result = render_swaync_config(
            b"{ not valid json ]",
            &[(SettingId::NotificationTimeout, Value::Integer(5))],
        );
        assert!(matches!(result, Err(SwayncRenderError::Parse(_))));
    }

    #[test]
    fn swaync_config_write_is_none_when_clean() {
        // No dirty Notifications settings -> Ok(None), no write (the common clean case).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        fs::write(&path, CONFIG_JSON).expect("write config.json");
        let model = NotificationsModel::load(path);
        assert!(matches!(model.swaync_config_write(&[]), Ok(None)));
    }

    #[test]
    fn swaync_config_write_errors_when_dirty_but_the_file_is_unreadable() {
        // A dirty Notifications edit against a missing file is an error (the
        // Apply aborts), never a silent skip that would let commit_apply promote an
        // unwritten value.
        let dir = tempfile::tempdir().expect("temp dir");
        let model = NotificationsModel::load(dir.path().join("gone.json"));
        let dirty = vec![(SettingId::NotificationTimeout, Value::Integer(5))];
        assert!(matches!(
            model.swaync_config_write(&dirty),
            Err(SwayncWriteError::Read(_))
        ));
    }

    #[test]
    fn dnd_command_builders_produce_the_exact_arg_vectors() {
        // The runtime DND commands are shell-free arg vectors against swaync-client, using
        // the flags confirmed against swaync-client(1).
        assert_eq!(
            dnd_query_command(),
            Command::new("swaync-client").arg("--get-dnd")
        );
        assert_eq!(
            set_dnd_command(true),
            Command::new("swaync-client").arg("--dnd-on")
        );
        assert_eq!(
            set_dnd_command(false),
            Command::new("swaync-client").arg("--dnd-off")
        );
    }

    #[test]
    fn dnd_state_parses_the_daemon_output() {
        // `--get-dnd` prints true/false; parse each, and degrade a failed command or
        // unparseable output to None (unknown) rather than panicking.
        let on = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0, "true\n", "",
        ))]);
        assert_eq!(dnd_state(&on), Some(true));
        assert_eq!(
            on.recorded(),
            vec![Command::new("swaync-client").arg("--get-dnd")]
        );

        let off = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0, "false\n", "",
        ))]);
        assert_eq!(dnd_state(&off), Some(false));

        // A non-zero exit (e.g. the daemon is not running) -> unknown.
        let failed = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        assert_eq!(dnd_state(&failed), None);

        // Unparseable stdout -> unknown, never a panic.
        let garbled = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0, "maybe\n", "",
        ))]);
        assert_eq!(dnd_state(&garbled), None);
    }

    #[test]
    fn set_dnd_issues_the_correct_swaync_client_command_and_stages_nothing() {
        // Accept (the DND gotcha): toggling DND issues exactly the right swaync-client
        // command immediately, with no config file and no store involvement — the function
        // takes only the command runner, so by construction it writes no config key and
        // stages nothing.
        let on = MockCommandRunner::new();
        set_dnd(&on, true);
        assert_eq!(
            on.recorded(),
            vec![Command::new("swaync-client").arg("--dnd-on")]
        );

        let off = MockCommandRunner::new();
        set_dnd(&off, false);
        assert_eq!(
            off.recorded(),
            vec![Command::new("swaync-client").arg("--dnd-off")]
        );
    }

    #[test]
    fn a_dirty_notifications_edit_applies_through_the_pipeline_with_swaync_reload() {
        // The end-to-end store-SettingId -> FileWrite glue (task 6.7): a dirty position +
        // timeout edit renders a surgical config.json FileWrite (JSON round-trip, stable
        // key order, only the edited keys changed), and applying it through the shared
        // pipeline writes that file and triggers exactly `swaync-client -rs` — the swaync
        // config reload (task 4.4) — and nothing else (R5.3).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        fs::write(&path, CONFIG_JSON).expect("write config.json");
        let model = NotificationsModel::load(path.clone());

        let dirty = vec![
            (
                SettingId::NotificationPosition,
                Value::Enum("bottom-left".to_string()),
            ),
            (SettingId::NotificationTimeout, Value::Integer(30)),
        ];
        let write = model
            .swaync_config_write(&dirty)
            .expect("no error rendering the write")
            .expect("a dirty Notifications setting produces a write");
        assert_eq!(write.path, path);
        assert_eq!(write.backing, BackingFile::SwayncConfig);
        assert_eq!(
            write.changed_keys,
            vec![
                "positionY".to_string(),
                "positionX".to_string(),
                "timeout".to_string()
            ]
        );

        // The freshness baseline matches the on-disk bytes, so the pipeline sees no
        // conflict.
        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("baseline config.json");

        let plan = ApplyPlan {
            validations: dirty,
            writes: vec![write],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        // swaync-client -rs is gated on the swaync daemon being live; no hyprctl is needed.
        let caps = Capabilities::for_tests(&[], &[Daemon::Swaync], false);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        match outcome {
            ApplyOutcome::Applied {
                reload_failures,
                written,
            } => {
                assert!(reload_failures.is_empty());
                assert_eq!(written, vec![path.clone()]);
            }
            other => panic!("expected Applied, got {other:?}"),
        }

        // On disk, only the three edited keys changed (JSON round-trip, stable key order).
        let on_disk = fs::read_to_string(&path).expect("read back");
        let expected = CONFIG_JSON
            .replace("\"positionX\": \"right\"", "\"positionX\": \"left\"")
            .replace("\"positionY\": \"top\"", "\"positionY\": \"bottom\"")
            .replace("\"timeout\": 10", "\"timeout\": 30");
        assert_eq!(on_disk, expected);

        // The Notifications change reloads only via `swaync-client -rs`.
        assert_eq!(
            runner.recorded(),
            vec![Command::new("swaync-client").arg("-rs")]
        );
        assert!(signaller.calls().is_empty());
    }

    #[test]
    fn a_second_apply_after_commit_is_not_a_self_conflict_through_the_real_glue() {
        // Mirrors the Input page's end-to-end self-conflict test (task 6.6): config.json is
        // store-loaded, so the window folds its write into `apply::run` over the store's
        // freshness and `commit_apply` re-baselines it from the exact bytes written — the
        // app's own first write must NOT read as an external change on the next apply
        // (R5.6). Load config.json into a real store, stage a position edit, apply + commit
        // through the real renderer, then stage a timeout edit and re-apply: `Applied`,
        // never `Conflicted`.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        fs::write(&path, CONFIG_JSON).expect("write config.json");
        let model = NotificationsModel::load(path.clone());

        // Load the real originals + freshness baseline, as the startup load does. A trivial
        // re-reader suffices (the conflict-reload path is not exercised here).
        let reader: FileReader = Box::new(|p| {
            Ok(FileValues {
                bytes: fs::read(p)?,
                values: Vec::new(),
            })
        });
        let mut store = SettingsStore::new();
        let bytes = fs::read(&path).expect("read config.json");
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: vec![
                    (
                        SettingId::NotificationPosition,
                        Value::Enum("top-right".to_string()),
                    ),
                    (SettingId::NotificationTimeout, Value::Integer(10)),
                ],
            },
            reader,
        );

        // swaync daemon live so the -rs reload is planned; no hyprctl needed.
        let caps = Capabilities::for_tests(&[], &[Daemon::Swaync], false);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        // First apply: change the position through the real glue.
        store
            .stage(
                SettingId::NotificationPosition,
                Value::Enum("bottom-left".to_string()),
            )
            .expect("stage the first edit");
        let dirty = store.dirty_in_category(Category::Notifications);
        let write = model
            .swaync_config_write(&dirty)
            .expect("no error")
            .expect("a write");
        let written_bytes = write.contents.clone();
        let plan = ApplyPlan {
            validations: dirty,
            writes: vec![write],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let written = match apply::run(&plan, store.freshness(), &caps, &runner, &signaller) {
            ApplyOutcome::Applied { written, .. } => written,
            other => panic!("expected Applied on the first apply, got {other:?}"),
        };
        assert_eq!(written, vec![path.clone()]);
        // Commit as the window does: re-baseline config.json from the exact bytes written.
        store.commit_apply(&[(path.clone(), written_bytes)]);

        // Second apply: change the timeout. The on-disk file is now the app's own first
        // write; the commit re-baselined it, so this must NOT self-conflict.
        store
            .stage(SettingId::NotificationTimeout, Value::Integer(30))
            .expect("stage the second edit");
        let dirty2 = store.dirty_in_category(Category::Notifications);
        let write2 = model
            .swaync_config_write(&dirty2)
            .expect("no error")
            .expect("a write");
        let plan2 = ApplyPlan {
            validations: dirty2,
            writes: vec![write2],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        assert!(
            matches!(
                apply::run(&plan2, store.freshness(), &caps, &runner, &signaller),
                ApplyOutcome::Applied { .. }
            ),
            "the second apply must not self-conflict after the first commit re-baselined \
             config.json through the real renderer"
        );

        // Both applied edits are on disk (the position from the first apply, the timeout
        // from the second).
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert!(
            on_disk.contains("\"positionX\": \"left\"")
                && on_disk.contains("\"positionY\": \"bottom\"")
                && on_disk.contains("\"timeout\": 30"),
            "both applied edits are on disk: {on_disk:?}"
        );
    }
}
