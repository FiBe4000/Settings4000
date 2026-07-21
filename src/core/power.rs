//! GTK-free Power & Idle-page domain logic (task 6.8; architecture §3, §6; R4.2,
//! R4.4, R5.6, R8.3, R6.2).
//!
//! # What this module is
//!
//! The Power & Idle page edits the three idle timeouts (dim / lock / DPMS) and the
//! session lock command, all staged to `config/hypr/hypridle.conf`. Like the Input and
//! Notifications pages these settings map cleanly onto the fixed
//! [`SettingId`](crate::core::model::SettingId) enum, so they are staged in the shared
//! [`SettingsStore`](crate::core::store) and rendered by the declarative row framework
//! (task 5.2 — three [`WidgetKind::Scale`](crate::ui::row::WidgetKind::Scale) sliders and
//! a [`WidgetKind::Entry`](crate::ui::row::WidgetKind::Entry) text field). This module
//! supplies the one Power-specific piece the store cannot: the **store-`SettingId` →
//! `hypridle.conf` write glue**. [`render_hypridle_conf`] takes the store's dirty Power &
//! Idle settings and applies each to the file through the surgical hyprlang writer
//! (§3.2), producing one lossless [`FileWrite`] whose diff is limited to the touched
//! value spans (mirroring [`crate::core::input::render_input_conf`] and
//! [`crate::core::notifications::render_swaync_config`]). The pipeline then restarts
//! hypridle so it re-reads its config (task 4.4).
//!
//! It lives in `core/` because every piece is pure domain logic over bytes — no GTK, no
//! process side effects — so it is unit-tested headlessly (R6.2); the layering guard in
//! `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import here.
//!
//! # Positional listener matching (the load-bearing assumption)
//!
//! `hypridle.conf` has a `general { }` block plus several **identically-named**
//! `listener { }` blocks, told apart only by their position in the file. The real
//! dotfiles config (analysis §6) lays them out in a fixed order:
//!
//! - `listener[0]` dims the screen (`on-timeout = brightnessctl …`);
//! - `listener[1]` locks the session (`on-timeout = loginctl lock-session`);
//! - `listener[2]` switches the displays off (`on-timeout = hyprctl dispatch dpms off`).
//!
//! So this module addresses each timeout by that **positional** assumption — the dim /
//! lock / DPMS sliders map to `listener[0]` / `listener[1]` / `listener[2]`'s `timeout`
//! key respectively — using the hyprlang parser's occurrence-indexed section addressing
//! ([`SectionStep::nth`], §3.2). Editing one listener's `timeout` therefore rewrites
//! only that occurrence's value span, leaving the other listener blocks byte-identical
//! (the headline task-6.8 acceptance criterion). The assumption is documented here and
//! pinned by [`power_key_path`], which both the read side ([`crate::ui::startup`]) and
//! the write side share, so a value round-trips through the same address it was parsed
//! from. A config whose listeners are in a different order would map the sliders to the
//! wrong blocks — inherent to positional matching, and why the ordering is called out
//! rather than inferred from each listener's `on-timeout` command (which is deliberately
//! not editable here).
//!
//! # Where the lock command lives
//!
//! The user-facing "lock command" is hypridle's `general { }` `lock_cmd` — the command
//! hypridle actually runs to lock the session (`pidof hyprlock || hyprlock` in the real
//! config). It is **not** `listener[1]`'s `on-timeout`, which is `loginctl lock-session`:
//! that only asks logind to emit a lock signal, which hypridle catches and answers by
//! running `lock_cmd`. So [`SettingId::LockCommand`] edits `general.lock_cmd`, verified
//! against the live file (analysis §6).
//!
//! # Conflict safety comes from the store (R5.6)
//!
//! `hypridle.conf` is loaded through the store at startup, which fingerprints it as the
//! freshness baseline and hands the same tracker to the Apply pipeline
//! ([`apply::run`](crate::core::apply::run)); a commit re-baselines it. So this module
//! deliberately owns **no** [`FreshnessTracker`](crate::core::freshness::FreshnessTracker):
//! [`PowerModel::hypridle_conf_write`] just reads the current bytes and renders the edit,
//! and the pipeline's step-2 conflict check aborts the apply if the file changed
//! externally — nothing here can clobber an external edit (the same contract as the Input
//! and Notifications pages).

use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::core::apply::FileWrite;
use crate::core::model::{SettingId, Value};
use crate::core::reload::BackingFile;
use crate::parsers::hyprlang::{EditError, HyprlangFile, KeyPath, SectionStep};

/// The hyprlang section name of a `listener { }` block in `hypridle.conf`.
const LISTENER_SECTION: &str = "listener";
/// The `timeout` key inside a `listener { }` block.
const TIMEOUT_KEY: &str = "timeout";

/// The 0-based occurrence of the `listener { }` block that dims the screen (analysis
/// §6). See the module docs on the positional-matching assumption.
const DIM_LISTENER: usize = 0;
/// The 0-based occurrence of the `listener { }` block that locks the session.
const LOCK_LISTENER: usize = 1;
/// The 0-based occurrence of the `listener { }` block that switches the displays off
/// (DPMS).
const DPMS_LISTENER: usize = 2;

/// The `hypridle.conf` address each Power & Idle [`SettingId`] edits, or `None` for a
/// setting the Power & Idle page does not back (a guard for a caller that passes a
/// foreign id).
///
/// This is the store-`SettingId` → hyprlang address map, the single definition of the
/// positional-matching assumption (see the module docs): the three timeouts resolve to
/// the `timeout` key of a specific `listener { }` occurrence, and the lock command to
/// `general.lock_cmd`. The read side ([`crate::ui::startup`]) uses this same map, so a
/// value is parsed from and written back to the identical address.
pub fn power_key_path(id: SettingId) -> Option<KeyPath> {
    match id {
        SettingId::DimTimeout => Some(listener_timeout_path(DIM_LISTENER)),
        SettingId::LockTimeout => Some(listener_timeout_path(LOCK_LISTENER)),
        SettingId::DpmsTimeout => Some(listener_timeout_path(DPMS_LISTENER)),
        SettingId::LockCommand => Some(KeyPath::at(&["general"], "lock_cmd")),
        _ => None,
    }
}

/// The [`KeyPath`] addressing the `timeout` key of the `occurrence`-th `listener { }`
/// block (0-based), via the parser's occurrence-indexed section addressing (§3.2).
fn listener_timeout_path(occurrence: usize) -> KeyPath {
    KeyPath::new(
        vec![SectionStep::nth(LISTENER_SECTION, occurrence)],
        TIMEOUT_KEY,
    )
}

/// The on-disk `hypridle.conf` string form of a Power & Idle setting's [`Value`], or
/// `None` when the value's kind does not match the setting (a guard, not an expected
/// case — the store validates kinds on stage).
///
/// A [`Value::Integer`] timeout is rendered as its plain decimal (`300`, never `300.0`);
/// the [`Value::String`] lock command is written verbatim.
fn value_to_hypr_string(id: SettingId, value: &Value) -> Option<String> {
    match (id, value) {
        (
            SettingId::DimTimeout | SettingId::LockTimeout | SettingId::DpmsTimeout,
            Value::Integer(seconds),
        ) => Some(seconds.to_string()),
        (SettingId::LockCommand, Value::String(text)) => Some(text.clone()),
        _ => None,
    }
}

/// The complete new `hypridle.conf` bytes plus the changed-key labels, produced by
/// applying the store's dirty Power & Idle edits to the current file (task 6.8).
///
/// Returned by [`render_hypridle_conf`] and wrapped in a [`FileWrite`] by
/// [`PowerModel::hypridle_conf_write`]. The `changed_keys` are the rendered section
/// paths (e.g. `listener[1].timeout`, `general.lock_cmd`) used only for the apply-level
/// log line (R7.3), never the file contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HypridleConfEdit {
    /// The complete new file contents (surgical, span-preserving — §3).
    pub contents: Vec<u8>,
    /// The section paths this edit changed, for logging.
    pub changed_keys: Vec<String>,
}

/// Why [`PowerModel::hypridle_conf_write`] could not produce a write despite there being
/// dirty Power & Idle settings to apply (task 6.8, the same abort-not-skip contract as
/// the Input page's `InputWriteError`).
///
/// This is distinct from "nothing was dirty" (a plain `Ok(None)`): when the user *has*
/// pending Power & Idle edits but the write cannot be rendered, the Apply must **abort**
/// rather than skip the write and let the store commit the staged values against an
/// unchanged file — that would desync the store from disk. Both cases are near-unreachable
/// in practice (`hypridle.conf` is readable and was parseable at load), but treating them
/// as failures keeps the store and the file in agreement (R8.3).
#[derive(Debug)]
pub enum PowerWriteError {
    /// `hypridle.conf` could not be read to render the edits.
    Read(io::Error),
    /// The hyprlang writer rejected an edit — a value it cannot represent (a `#` or
    /// newline in the lock command, R8.3), or a missing `listener`/`general` section to
    /// write into.
    Render(EditError),
}

impl fmt::Display for PowerWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PowerWriteError::Read(error) => write!(f, "hypridle.conf could not be read: {error}"),
            PowerWriteError::Render(error) => {
                write!(f, "the hypridle.conf edit could not be applied: {error}")
            }
        }
    }
}

impl std::error::Error for PowerWriteError {}

/// Applies the store's dirty Power & Idle `edits` to the current `hypridle.conf` `bytes`,
/// returning the complete new bytes (task 6.8; R5.3 item 1).
///
/// This is the pure store-`SettingId` → `hypridle.conf` glue. It parses the current file
/// losslessly, then for each `(id, value)` rewrites **only** that setting's value span
/// through the surgical hyprlang writer ([`HyprlangFile::set_value`]) at the address
/// [`power_key_path`] gives it — so comments, ordering, unrelated keys, and every
/// untouched byte stay identical and the emitted diff is limited to the changed lines.
/// Because each timeout addresses a specific `listener { }` occurrence (§3.2), editing
/// one leaves the other listener blocks byte-identical (the headline task-6.8 criterion).
///
/// Returns an [`EditError`] if the writer rejects an edit — a lock command containing a
/// newline/`#` (R8.3), or an addressed `listener`/`general` section that does not exist —
/// leaving the caller to abort the apply rather than emit a partial file. A setting with
/// no Power & Idle address is ignored (it does not belong to this file).
pub fn render_hypridle_conf(
    bytes: &[u8],
    edits: &[(SettingId, Value)],
) -> Result<HypridleConfEdit, EditError> {
    let text = String::from_utf8_lossy(bytes);
    let (mut file, _warnings) = HyprlangFile::parse(&text);

    let mut changed_keys = Vec::new();
    for (id, value) in edits {
        let (Some(path), Some(rendered)) = (power_key_path(*id), value_to_hypr_string(*id, value))
        else {
            // Not a Power & Idle-backed setting, or a kind mismatch the store would have
            // rejected on stage: skip rather than write a bad value.
            continue;
        };
        file.set_value(&path, &rendered)?;
        changed_keys.push(path.to_string());
    }

    Ok(HypridleConfEdit {
        contents: file.emit().into_bytes(),
        changed_keys,
    })
}

/// The Power & Idle page's Apply-time write glue for `hypridle.conf` (task 6.8).
///
/// Built once on the startup worker when hypridle is present, and held by the window.
/// Like the Input and Notifications pages' helpers it is **not** a staging model — the
/// three timeouts and the lock command are staged in the shared store, so dirty tracking,
/// validation, reset, and commit all flow through the store — and it owns **no**
/// [`FreshnessTracker`](crate::core::freshness::FreshnessTracker): `hypridle.conf` is
/// store-loaded, so the store baselines it and the Apply pipeline conflict-checks it. This
/// just turns the store's dirty Power & Idle settings into the one `hypridle.conf`
/// [`FileWrite`] on Apply, which the pipeline then follows with a hypridle restart
/// (task 4.4).
pub struct PowerModel {
    /// The live XDG path of `hypridle.conf` (R8.5), read fresh when rendering a write.
    hypridle_conf: PathBuf,
}

impl PowerModel {
    /// Builds the model, recording the `hypridle.conf` path (the production entry point,
    /// called from the startup worker — architecture §8).
    pub fn load(hypridle_conf: PathBuf) -> PowerModel {
        PowerModel { hypridle_conf }
    }

    /// Renders the store's dirty Power & Idle settings into a `hypridle.conf`
    /// [`FileWrite`] (task 6.8).
    ///
    /// `dirty` is the store's dirty Power & Idle settings (from
    /// [`SettingsStore::dirty_in_category`](crate::core::store::SettingsStore::dirty_in_category)).
    /// It reads the current file bytes and applies the edits through
    /// [`render_hypridle_conf`], returning a single surgical [`FileWrite`] for the shared
    /// Apply pipeline. Reading fresh each time — rather than caching a parsed copy — is
    /// what keeps it correct across repeated applies and external edits without a bespoke
    /// freshness tracker: the pipeline's conflict check (against the store's baseline)
    /// aborts the apply if the file changed since load, so a fresh read can never clobber
    /// an external edit (the same contract as the Input and Notifications models).
    ///
    /// Returns:
    /// - `Ok(None)` when there is nothing to write — `dirty` is empty (the common clean
    ///   case). This is what makes the hypridle restart fire **only** when the file
    ///   actually changed: with no write, the pipeline derives no `HypridleConf` change
    ///   and plans no restart.
    /// - `Ok(Some(write))` with the rendered write;
    /// - `Err(PowerWriteError)` when there *are* dirty Power & Idle settings but the write
    ///   cannot be produced (the file is unreadable, or the writer rejects an edit). The
    ///   caller must **abort the Apply** rather than skip the write, since the store would
    ///   otherwise commit the staged values against an unchanged file and desync. Both
    ///   failure modes are near-unreachable in practice.
    pub fn hypridle_conf_write(
        &self,
        dirty: &[(SettingId, Value)],
    ) -> Result<Option<FileWrite>, PowerWriteError> {
        if dirty.is_empty() {
            return Ok(None);
        }
        let bytes = std::fs::read(&self.hypridle_conf).map_err(|error| {
            tracing::error!(
                path = %self.hypridle_conf.display(),
                %error,
                "could not read hypridle.conf to render the Power & Idle edits; \
                 aborting the apply (R8.3)"
            );
            PowerWriteError::Read(error)
        })?;
        let edit = render_hypridle_conf(&bytes, dirty).map_err(|error| {
            tracing::error!(
                path = %self.hypridle_conf.display(),
                %error,
                "failed to render a hypridle.conf edit; aborting the apply (R8.3)"
            );
            PowerWriteError::Render(error)
        })?;
        // Defensive: every dirty Power & Idle setting maps to an address and a rendered
        // value, so `changed_keys` is non-empty here — this branch is effectively dead,
        // but is guarded rather than emitting a byte-identical no-op write.
        if edit.changed_keys.is_empty() {
            return Ok(None);
        }
        Ok(Some(FileWrite {
            path: self.hypridle_conf.clone(),
            contents: edit.contents,
            changed_keys: edit.changed_keys,
            backing: BackingFile::HypridleConf,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    use crate::core::apply::{self, ApplyOutcome, ApplyPlan};
    use crate::core::detect::{Capabilities, Daemon};
    use crate::core::freshness::FreshnessTracker;
    use crate::core::model::Category;
    use crate::core::reload::ReloadParams;
    use crate::core::store::{FileReader, FileValues, SettingsStore};
    use crate::system::command::{Command, MockCommandRunner};
    use crate::system::signal::MockProcessSignaller;

    /// A realistic `hypridle.conf` fixture in the real dotfiles' shape (analysis §6): a
    /// `general { }` block with the lock command, then three `listener { }` blocks in the
    /// dim / lock / DPMS order, each carrying an inline comment so comment preservation is
    /// exercised.
    const HYPRIDLE_CONF: &str = "\
general {
    lock_cmd = pidof hyprlock || hyprlock
    before_sleep_cmd = loginctl lock-session
    after_sleep_cmd = hyprctl dispatch dpms on
}

listener {
    timeout = 150                                # 2.5min.
    on-timeout = brightnessctl -s set 10         # dim the backlight.
    on-resume = brightnessctl -r                 # restore the backlight.
}

listener {
    timeout = 300                                # 5min
    on-timeout = loginctl lock-session           # lock the session.
}

listener {
    timeout = 330                                # 5.5min
    on-timeout = hyprctl dispatch dpms off       # screen off.
    on-resume = hyprctl dispatch dpms on         # screen on.
}
";

    /// The indices at which two texts' lines differ — to assert an edit touched exactly
    /// the expected lines (mirrors the Input page's own edit tests).
    fn differing_lines(before: &str, after: &str) -> Vec<usize> {
        let before: Vec<&str> = before.lines().collect();
        let after: Vec<&str> = after.lines().collect();
        assert_eq!(
            before.len(),
            after.len(),
            "a surgical edit must not add or remove lines"
        );
        before
            .iter()
            .zip(&after)
            .enumerate()
            .filter_map(|(i, (b, a))| (b != a).then_some(i))
            .collect()
    }

    /// Renders `edits` into `HYPRIDLE_CONF` and returns the emitted text.
    fn render(edits: &[(SettingId, Value)]) -> String {
        let edit = render_hypridle_conf(HYPRIDLE_CONF.as_bytes(), edits).expect("edits render");
        String::from_utf8(edit.contents).expect("valid utf-8")
    }

    #[test]
    fn each_control_maps_to_the_right_listener_or_general_address() {
        // The store-SettingId -> hypridle.conf address map (the positional assumption):
        // the three timeouts resolve to a specific `listener` occurrence's `timeout`, and
        // the lock command to `general.lock_cmd`. A foreign setting has no address here.
        assert_eq!(
            power_key_path(SettingId::DimTimeout).map(|p| p.to_string()),
            Some("listener.timeout".to_string()),
            "dim maps to the first listener (occurrence 0 renders without an index)"
        );
        assert_eq!(
            power_key_path(SettingId::LockTimeout).map(|p| p.to_string()),
            Some("listener[1].timeout".to_string())
        );
        assert_eq!(
            power_key_path(SettingId::DpmsTimeout).map(|p| p.to_string()),
            Some("listener[2].timeout".to_string())
        );
        assert_eq!(
            power_key_path(SettingId::LockCommand).map(|p| p.to_string()),
            Some("general.lock_cmd".to_string())
        );
        assert_eq!(power_key_path(SettingId::NotificationTimeout), None);
    }

    #[test]
    fn editing_one_listener_timeout_leaves_the_other_blocks_byte_identical() {
        // THE headline acceptance criterion: editing the dim listener's timeout rewrites
        // only that occurrence's value span; the lock and DPMS listener blocks (and the
        // general block, and every comment) stay byte-identical — occurrence-indexed
        // surgical addressing (§3.2).
        let edited = render(&[(SettingId::DimTimeout, Value::Integer(90))]);
        let target = HYPRIDLE_CONF
            .lines()
            .position(|l| l == "    timeout = 150                                # 2.5min.")
            .expect("fixture has the dim listener timeout");
        assert_eq!(
            differing_lines(HYPRIDLE_CONF, &edited),
            vec![target],
            "only the first listener's timeout line changes"
        );
        // The value span changed but the inline comment and spacing before it survive.
        assert_eq!(
            edited.lines().nth(target),
            Some("    timeout = 90                                # 2.5min.")
        );
        // The other two listeners' timeouts are untouched.
        assert!(edited.contains("    timeout = 300"));
        assert!(edited.contains("    timeout = 330"));
    }

    #[test]
    fn each_timeout_maps_to_its_own_listener() {
        // Positional matching against the multi-listener fixture: the lock slider edits
        // only listener[1], and the DPMS slider only listener[2] — never each other's
        // block.
        let lock = render(&[(SettingId::LockTimeout, Value::Integer(600))]);
        let lock_line = HYPRIDLE_CONF
            .lines()
            .position(|l| l == "    timeout = 300                                # 5min")
            .expect("fixture has the lock listener timeout");
        assert_eq!(differing_lines(HYPRIDLE_CONF, &lock), vec![lock_line]);
        assert_eq!(
            lock.lines().nth(lock_line),
            Some("    timeout = 600                                # 5min")
        );

        let dpms = render(&[(SettingId::DpmsTimeout, Value::Integer(900))]);
        let dpms_line = HYPRIDLE_CONF
            .lines()
            .position(|l| l == "    timeout = 330                                # 5.5min")
            .expect("fixture has the dpms listener timeout");
        assert_eq!(differing_lines(HYPRIDLE_CONF, &dpms), vec![dpms_line]);
        assert_eq!(
            dpms.lines().nth(dpms_line),
            Some("    timeout = 900                                # 5.5min")
        );
    }

    #[test]
    fn a_lock_command_edit_changes_only_the_general_lock_cmd() {
        // The lock command edits `general.lock_cmd` (not a listener's on-timeout), and only
        // that line's value span changes; the `||` in the original value is no obstacle.
        let edited = render(&[(
            SettingId::LockCommand,
            Value::String("hyprlock --immediate".to_string()),
        )]);
        let target = HYPRIDLE_CONF
            .lines()
            .position(|l| l == "    lock_cmd = pidof hyprlock || hyprlock")
            .expect("fixture has the lock command");
        assert_eq!(differing_lines(HYPRIDLE_CONF, &edited), vec![target]);
        assert_eq!(
            edited.lines().nth(target),
            Some("    lock_cmd = hyprlock --immediate")
        );
    }

    #[test]
    fn a_full_edit_changes_only_the_touched_lines_in_setting_id_order() {
        // All four Power & Idle settings at once: each touches exactly its own line, and
        // `changed_keys` is in SettingId order (dim, lock, dpms, then the lock command).
        let edit = render_hypridle_conf(
            HYPRIDLE_CONF.as_bytes(),
            &[
                (SettingId::DimTimeout, Value::Integer(60)),
                (SettingId::LockTimeout, Value::Integer(300)),
                (SettingId::DpmsTimeout, Value::Integer(360)),
                (
                    SettingId::LockCommand,
                    Value::String("hyprlock".to_string()),
                ),
            ],
        )
        .expect("full edit renders");
        assert_eq!(
            edit.changed_keys,
            vec![
                "listener.timeout".to_string(),
                "listener[1].timeout".to_string(),
                "listener[2].timeout".to_string(),
                "general.lock_cmd".to_string(),
            ]
        );
    }

    #[test]
    fn no_edits_round_trips_the_file_byte_for_byte() {
        // With no dirty settings the render is the identity (nothing touched).
        let edit = render_hypridle_conf(HYPRIDLE_CONF.as_bytes(), &[]).expect("renders");
        assert_eq!(edit.contents, HYPRIDLE_CONF.as_bytes());
        assert!(edit.changed_keys.is_empty());
    }

    #[test]
    fn a_foreign_setting_is_skipped() {
        // A setting that is not a Power & Idle address (a caller passing a foreign id) is
        // ignored — it does not belong to this file, so nothing changes.
        let edit = render_hypridle_conf(
            HYPRIDLE_CONF.as_bytes(),
            &[(SettingId::MouseSensitivity, Value::Float(0.5))],
        )
        .expect("renders");
        assert_eq!(edit.contents, HYPRIDLE_CONF.as_bytes());
        assert!(edit.changed_keys.is_empty());
    }

    #[test]
    fn render_errors_when_the_addressed_listener_is_missing() {
        // A config with fewer listeners than the positional assumption expects: editing
        // the DPMS timeout (listener[2]) when only one listener exists surfaces a
        // SectionNotFound error, so the Apply aborts rather than emit a partial file.
        const ONE_LISTENER: &str =
            "general {\n    lock_cmd = hyprlock\n}\n\nlistener {\n    timeout = 150\n}\n";
        let result = render_hypridle_conf(
            ONE_LISTENER.as_bytes(),
            &[(SettingId::DpmsTimeout, Value::Integer(900))],
        );
        assert!(matches!(result, Err(EditError::SectionNotFound(_))));
    }

    #[test]
    fn hypridle_conf_write_is_none_when_clean() {
        // No dirty Power & Idle settings -> Ok(None), no write (the common clean case).
        // This is also what makes the restart fire only on a real change: no write means
        // no HypridleConf in the reload set.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hypridle.conf");
        fs::write(&path, HYPRIDLE_CONF).expect("write hypridle.conf");
        let model = PowerModel::load(path);
        assert!(matches!(model.hypridle_conf_write(&[]), Ok(None)));
    }

    #[test]
    fn hypridle_conf_write_errors_when_dirty_but_the_file_is_unreadable() {
        // A dirty Power & Idle edit against a missing file is an error (the Apply aborts),
        // never a silent skip that would let commit_apply promote an unwritten value.
        let dir = tempfile::tempdir().expect("temp dir");
        let model = PowerModel::load(dir.path().join("gone.conf"));
        let dirty = vec![(SettingId::DimTimeout, Value::Integer(90))];
        assert!(matches!(
            model.hypridle_conf_write(&dirty),
            Err(PowerWriteError::Read(_))
        ));
    }

    #[test]
    fn hypridle_conf_write_rejects_an_unsafe_lock_command_defense_in_depth() {
        // Defense in depth: even though the stage-time validator rejects a lock
        // command containing `#`/newline before it can be staged, the writer keeps its own
        // `reject_unsafe_value` guard — a lock command with a `#` surfaces as a
        // `PowerWriteError::Render` here, so a value that somehow reached this point
        // (bypassing stage validation) still aborts the Apply rather than truncating the
        // config at the `#`.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hypridle.conf");
        fs::write(&path, HYPRIDLE_CONF).expect("write hypridle.conf");
        let model = PowerModel::load(path);
        let dirty = vec![(
            SettingId::LockCommand,
            Value::String("hyprlock # note".to_string()),
        )];
        assert!(matches!(
            model.hypridle_conf_write(&dirty),
            Err(PowerWriteError::Render(EditError::InvalidValue(_)))
        ));
    }

    #[test]
    fn a_dirty_power_edit_applies_through_the_pipeline_with_a_hypridle_restart() {
        // The end-to-end store-SettingId -> FileWrite glue (task 6.8): a dirty timeout +
        // lock-command edit renders a surgical hypridle.conf FileWrite whose diff is limited
        // to the touched lines, and applying it through the shared pipeline writes that file
        // and restarts hypridle (task 4.4) — nothing else. With a fresh MockCommandRunner
        // every command "succeeds", so `systemctl --user is-active` reports active and the
        // restart takes the systemd path (R5.3).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hypridle.conf");
        fs::write(&path, HYPRIDLE_CONF).expect("write hypridle.conf");
        let model = PowerModel::load(path.clone());

        let dirty = vec![
            (SettingId::LockTimeout, Value::Integer(600)),
            (
                SettingId::LockCommand,
                Value::String("hyprlock".to_string()),
            ),
        ];
        let write = model
            .hypridle_conf_write(&dirty)
            .expect("no error rendering the write")
            .expect("a dirty Power & Idle setting produces a write");
        assert_eq!(write.path, path);
        assert_eq!(write.backing, BackingFile::HypridleConf);
        assert_eq!(
            write.changed_keys,
            vec![
                "listener[1].timeout".to_string(),
                "general.lock_cmd".to_string(),
            ]
        );

        // The freshness baseline matches the on-disk bytes, so the pipeline sees no
        // conflict.
        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("baseline hypridle.conf");

        let plan = ApplyPlan {
            validations: dirty,
            writes: vec![write],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        // The hypridle restart is gated on the hypridle daemon being live; no hyprctl is
        // needed for it.
        let caps = Capabilities::for_tests(&[], &[Daemon::Hypridle], false);
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

        // On disk, only the lock listener's timeout and the general lock command changed.
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert!(on_disk.contains("    timeout = 600"));
        assert!(on_disk.contains("    lock_cmd = hyprlock\n"));
        // The other listeners' timeouts are untouched (surgical, occurrence-indexed).
        assert!(on_disk.contains("    timeout = 150"));
        assert!(on_disk.contains("    timeout = 330"));

        // The Power & Idle change restarts hypridle via its active systemd user unit
        // (task 4.4): is-active decides the path, then restart.
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("systemctl").args(["--user", "is-active", "--quiet", "hypridle"]),
                Command::new("systemctl").args(["--user", "restart", "hypridle"]),
            ]
        );
    }

    #[test]
    fn no_restart_is_issued_when_hypridle_conf_did_not_change() {
        // Accept criterion: the restart is issued ONLY when hypridle.conf changed. With no
        // dirty Power & Idle setting the model produces no write, so the plan carries no
        // HypridleConf change and the pipeline runs no reload command at all.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hypridle.conf");
        fs::write(&path, HYPRIDLE_CONF).expect("write hypridle.conf");
        let model = PowerModel::load(path.clone());

        // Clean: no write.
        assert!(matches!(model.hypridle_conf_write(&[]), Ok(None)));

        // A plan with no writes (the clean Power & Idle case) reloads nothing.
        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("baseline hypridle.conf");
        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: Vec::new(),
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let caps = Capabilities::for_tests(&[], &[Daemon::Hypridle], false);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        assert!(matches!(outcome, ApplyOutcome::Applied { .. }));
        assert!(
            runner.recorded().is_empty(),
            "hypridle must not be restarted when its config did not change"
        );
        assert!(signaller.calls().is_empty());
    }

    #[test]
    fn a_second_apply_after_commit_is_not_a_self_conflict_through_the_real_glue() {
        // Mirrors the Input/Notifications end-to-end self-conflict tests (tasks 6.6/6.7):
        // hypridle.conf is store-loaded, so the window folds its write into `apply::run`
        // over the store's freshness and `commit_apply` re-baselines it from the exact
        // bytes written — the app's own first write must NOT read as an external change on
        // the next apply (R5.6). Load hypridle.conf into a real store, stage a timeout edit,
        // apply + commit through the real renderer, then stage a lock-command edit and
        // re-apply: `Applied`, never `Conflicted`.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hypridle.conf");
        fs::write(&path, HYPRIDLE_CONF).expect("write hypridle.conf");
        let model = PowerModel::load(path.clone());

        // Load the real originals + freshness baseline, as the startup load does. A trivial
        // re-reader suffices (the conflict-reload path is not exercised here).
        let reader: FileReader = Box::new(|p: &Path| {
            Ok(FileValues {
                bytes: fs::read(p)?,
                values: Vec::new(),
            })
        });
        let mut store = SettingsStore::new();
        let bytes = fs::read(&path).expect("read hypridle.conf");
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: vec![
                    (SettingId::DimTimeout, Value::Integer(150)),
                    (SettingId::LockTimeout, Value::Integer(300)),
                    (SettingId::DpmsTimeout, Value::Integer(330)),
                    (
                        SettingId::LockCommand,
                        Value::String("pidof hyprlock || hyprlock".to_string()),
                    ),
                ],
            },
            reader,
        );

        // hypridle daemon live so the restart is planned.
        let caps = Capabilities::for_tests(&[], &[Daemon::Hypridle], false);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        // First apply: change the DPMS timeout through the real glue.
        store
            .stage(SettingId::DpmsTimeout, Value::Integer(360))
            .expect("stage the first edit");
        let dirty = store.dirty_in_category(Category::PowerAndIdle);
        let write = model
            .hypridle_conf_write(&dirty)
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
        // Commit as the window does: re-baseline hypridle.conf from the exact bytes written.
        store.commit_apply(&[(path.clone(), written_bytes)]);

        // Second apply: change the lock command. The on-disk file is now the app's own
        // first write; the commit re-baselined it, so this must NOT self-conflict.
        store
            .stage(
                SettingId::LockCommand,
                Value::String("hyprlock".to_string()),
            )
            .expect("stage the second edit");
        let dirty2 = store.dirty_in_category(Category::PowerAndIdle);
        let write2 = model
            .hypridle_conf_write(&dirty2)
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
             hypridle.conf through the real renderer"
        );

        // Both applied edits are on disk (the DPMS timeout from the first apply, the lock
        // command from the second).
        let on_disk = fs::read_to_string(&path).expect("read back");
        assert!(
            on_disk.contains("    timeout = 360") && on_disk.contains("    lock_cmd = hyprlock\n"),
            "both applied edits are on disk: {on_disk:?}"
        );
    }
}
