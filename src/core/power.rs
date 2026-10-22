//! GTK-free Power & Idle-page domain logic (task 6.8; architecture §3, §6; R4.2,
//! R4.4, R5.6, R8.3, R6.2).
//!
//! # What this module is
//!
//! The Power & Idle page edits the three idle timeouts (dim / lock / DPMS), the
//! session lock command and hyprlock's grace period, all staged to
//! `config/hypr/hypridle.conf`. Like the Input page these settings map cleanly onto the
//! fixed [`SettingId`](crate::core::model::SettingId) enum, so they are staged in the
//! shared [`SettingsStore`](crate::core::store) and rendered by the declarative row
//! framework (task 5.2 — three [`WidgetKind::Scale`](crate::ui::row::WidgetKind::Scale)
//! sliders, a [`WidgetKind::Entry`](crate::ui::row::WidgetKind::Entry) text field and a
//! `WidgetKind::Spin` spin button for the grace period, all in `ui::row`). This module
//! supplies the one Power-specific piece the store cannot: the **store-`SettingId` →
//! `hypridle.conf` write glue**. [`render_hypridle_conf`] takes the store's dirty Power &
//! Idle settings and applies each to the file through the surgical hyprlang writer
//! (§3.2), producing one lossless [`FileWrite`] whose diff is limited to the touched
//! value spans (mirroring [`crate::core::input::render_input_lua`]). The pipeline then
//! restarts hypridle so it re-reads its config (task 4.4).
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
//! pinned by [`power_key_path`], which both the read side
//! ([`crate::core::load::load_hypridle_conf`]) and the write side share, so a value
//! round-trips through the same address it was parsed from. A config whose listeners
//! are in a different order would map the sliders to the wrong blocks — inherent to
//! positional matching, and why the ordering is called out rather than inferred from
//! each listener's `on-timeout` command (which is deliberately not editable here).
//!
//! # Where the lock command lives
//!
//! The user-facing "lock command" is hypridle's `general { }` `lock_cmd` — the command
//! hypridle actually runs to lock the session (`pidof hyprlock || hyprlock --grace 10` in the real
//! config). It is **not** `listener[1]`'s `on-timeout`, which is `loginctl lock-session`:
//! that only asks logind to emit a lock signal, which hypridle catches and answers by
//! running `lock_cmd`. So [`SettingId::LockCommand`] edits `general.lock_cmd`, verified
//! against the live file (analysis §6).
//!
//! # The grace period is a token inside the lock command
//!
//! hyprlock ≥ 0.9 takes its grace period only on the command line, so
//! [`SettingId::LockGracePeriod`] has no key of its own: it is the `--grace N` token of
//! that same `general.lock_cmd` value (`pidof hyprlock || hyprlock --grace 10`, analysis
//! §7, §8). [`power_key_path`] therefore gives both settings the same address, and
//! [`render_hypridle_conf`] writes the grace period as a token edit on the value string
//! ([`set_grace`], architecture §3), leaving the rest of the command and the line's
//! trailing comment untouched.
//!
//! When the lock command and the grace period are both dirty in one Apply, the **lock
//! command is written first and the grace token is then applied to the result**. The
//! store already keeps the two in step whenever either is staged (see the store's docs on
//! linked settings), so in practice the second step finds its number in place and
//! changes nothing; the fixed order is what makes the outcome well-defined for any
//! caller, and the one `general.lock_cmd` line is reported as changed once.
//!
//! # Conflict safety comes from the store (R5.6)
//!
//! `hypridle.conf` is loaded through the store at startup, which fingerprints it as the
//! freshness baseline and hands the same tracker to the Apply pipeline
//! ([`apply::run`](crate::core::apply::run)); a commit re-baselines it. So this module
//! deliberately owns **no** [`FreshnessTracker`](crate::core::freshness::FreshnessTracker):
//! [`PowerModel::apply_contribution`] just reads the current bytes and renders the edit,
//! and the pipeline's step-2 conflict check aborts the apply if the file changed
//! externally — nothing here can clobber an external edit (the same contract as the Input
//! page).

use std::fmt;
use std::path::PathBuf;

use crate::core::apply::{FileWrite, WriteValidation};
use crate::core::lock_grace::{GraceError, set_grace};
use crate::core::model::{SettingId, Value};
use crate::core::reload::BackingFile;
use crate::core::store_write::{RenderedEdit, StoreBackedFile, StoreWriteError};
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
/// `general.lock_cmd`. The grace period resolves to `general.lock_cmd` as well, because
/// it is a token inside that value rather than a key (see the module docs). The read
/// side ([`crate::core::load::load_hypridle_conf`]) uses this same map, so a value is
/// parsed from and written back to the identical address.
pub fn power_key_path(id: SettingId) -> Option<KeyPath> {
    match id {
        SettingId::DimTimeout => Some(listener_timeout_path(DIM_LISTENER)),
        SettingId::LockTimeout => Some(listener_timeout_path(LOCK_LISTENER)),
        SettingId::DpmsTimeout => Some(listener_timeout_path(DPMS_LISTENER)),
        SettingId::LockCommand | SettingId::LockGracePeriod => Some(lock_cmd_path()),
        _ => None,
    }
}

/// The [`KeyPath`] of `general.lock_cmd`, the one value the lock command and the grace
/// period share. A function of its own so [`apply_grace`] can address it directly
/// instead of unwrapping [`power_key_path`]'s `Option`.
fn lock_cmd_path() -> KeyPath {
    KeyPath::at(&["general"], "lock_cmd")
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

/// Applies the store's dirty Power & Idle `edits` to the current `hypridle.conf` `bytes`,
/// returning the complete new bytes plus the rendered section paths it changed (e.g.
/// `listener[1].timeout`, `general.lock_cmd`) for the apply-level log line (task 6.8;
/// R5.3 item 1). Each path is listed once, even when the lock command and the grace
/// period both changed its one line.
///
/// This is the pure store-`SettingId` → `hypridle.conf` glue. It parses the current file
/// losslessly, then for each `(id, value)` rewrites **only** that setting's value span
/// through the surgical hyprlang writer ([`HyprlangFile::set_value`]) at the address
/// [`power_key_path`] gives it — so comments, ordering, unrelated keys, and every
/// untouched byte stay identical and the emitted diff is limited to the changed lines.
/// Because each timeout addresses a specific `listener { }` occurrence (§3.2), editing
/// one leaves the other listener blocks byte-identical (the headline task-6.8 criterion).
///
/// The grace period ([`SettingId::LockGracePeriod`]) is applied **after** every other
/// edit, as a token edit ([`set_grace`]) on the `general.lock_cmd` value those edits
/// leave — so with a lock-command edit in the same batch, the new command is written
/// first and its grace token then set (see the module docs). A grace edit that leaves
/// the value unchanged reports no changed key.
///
/// Returns a [`HypridleRenderError`] if an edit is refused — a lock command containing a
/// newline/`#` (R8.3), an addressed `listener`/`general` section that does not exist, or
/// a grace period for a lock command that does not start hyprlock in a shape the token
/// editor accepts — leaving the caller to abort the apply rather than emit a partial
/// file. A setting with no Power & Idle address is ignored (it does not belong to this
/// file).
pub fn render_hypridle_conf(
    bytes: &[u8],
    edits: &[(SettingId, Value)],
) -> Result<RenderedEdit, HypridleRenderError> {
    let text = String::from_utf8_lossy(bytes);
    let (mut file, _warnings) = HyprlangFile::parse(&text);

    let mut changed_keys = Vec::new();
    let mut grace = None;
    for (id, value) in edits {
        if *id == SettingId::LockGracePeriod {
            // Applied after the loop, on top of whatever lock command this batch leaves.
            grace = Some(value);
            continue;
        }
        let (Some(path), Some(rendered)) = (power_key_path(*id), value_to_hypr_string(*id, value))
        else {
            // Not a Power & Idle-backed setting, or a kind mismatch the store would have
            // rejected on stage: skip rather than write a bad value.
            continue;
        };
        file.set_value(&path, &rendered)?;
        changed_keys.push(path.to_string());
    }

    if let Some(value) = grace {
        if let Some(path) = apply_grace(&mut file, value)? {
            let key = path.to_string();
            if !changed_keys.contains(&key) {
                changed_keys.push(key);
            }
        }
    }

    Ok(RenderedEdit {
        contents: file.emit().into_bytes(),
        changed_keys,
    })
}

/// Sets the grace token of `file`'s current `general.lock_cmd` value to the staged
/// grace `value`, returning the edited address, or `None` when the value already
/// carried that grace period (nothing to write).
fn apply_grace(
    file: &mut HyprlangFile,
    value: &Value,
) -> Result<Option<KeyPath>, HypridleRenderError> {
    let seconds = value
        .as_integer()
        .and_then(|seconds| u32::try_from(seconds).ok())
        .ok_or_else(|| HypridleRenderError::GraceValue(value.clone()))?;
    let path = lock_cmd_path();
    let current = file
        .value(&path)
        .ok_or(HypridleRenderError::MissingLockCommand)?
        .to_string();
    let edited = set_grace(&current, seconds)?;
    if edited == current {
        return Ok(None);
    }
    file.set_value(&path, &edited)?;
    Ok(Some(path))
}

/// Why [`render_hypridle_conf`] could not apply the staged Power & Idle edits to
/// `hypridle.conf`. Its message is quoted in the Apply-failure dialog, prefixed with the
/// file name.
#[derive(Clone, Debug, PartialEq)]
pub enum HypridleRenderError {
    /// The hyprlang writer refused an edit (see [`EditError`]).
    Edit(EditError),
    /// A grace period was staged, but the lock command offers no `--grace N` token the
    /// app can edit (see [`GraceError`]). The store refuses such an edit when it is
    /// staged; this is the backstop for a lock command changed on disk since.
    Grace(GraceError),
    /// A grace period was staged, but the file has no `general.lock_cmd` to carry it.
    MissingLockCommand,
    /// The staged grace value is not a non-negative whole number of seconds — a kind or
    /// range the store rejects on stage, guarded here rather than written.
    GraceValue(Value),
}

impl fmt::Display for HypridleRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HypridleRenderError::Edit(error) => write!(f, "{error}"),
            HypridleRenderError::Grace(error) => {
                write!(f, "the grace period cannot be written: {error}")
            }
            HypridleRenderError::MissingLockCommand => write!(
                f,
                "the grace period cannot be written: there is no `lock_cmd` in the `general` section"
            ),
            HypridleRenderError::GraceValue(Value::Integer(seconds)) => write!(
                f,
                "{seconds} is not a whole, non-negative number of seconds for the grace period"
            ),
            HypridleRenderError::GraceValue(other) => write!(
                f,
                "a {} value cannot be a grace period, which is a whole number of seconds",
                other.kind()
            ),
        }
    }
}

impl std::error::Error for HypridleRenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HypridleRenderError::Edit(error) => Some(error),
            HypridleRenderError::Grace(error) => Some(error),
            HypridleRenderError::MissingLockCommand | HypridleRenderError::GraceValue(_) => None,
        }
    }
}

impl From<EditError> for HypridleRenderError {
    fn from(error: EditError) -> Self {
        HypridleRenderError::Edit(error)
    }
}

impl From<GraceError> for HypridleRenderError {
    fn from(error: GraceError) -> Self {
        HypridleRenderError::Grace(error)
    }
}

/// The Power & Idle page's Apply-time write glue for `hypridle.conf` (task 6.8).
///
/// Built once on the startup worker when hypridle is present, and held by the window.
/// Like the Input page's helper it is **not** a staging model — the
/// three timeouts, the lock command and its grace period are staged in the shared store,
/// so dirty tracking,
/// validation, reset, and commit all flow through the store — and it owns **no**
/// [`FreshnessTracker`](crate::core::freshness::FreshnessTracker): `hypridle.conf` is
/// store-loaded, so the store baselines it and the Apply pipeline conflict-checks it. This
/// just turns the store's dirty Power & Idle settings into the one `hypridle.conf`
/// [`FileWrite`] on Apply, which the pipeline then follows with a hypridle restart
/// (task 4.4).
pub struct PowerModel {
    /// The `hypridle.conf` write target: its live XDG path (R8.5), read fresh when
    /// rendering a write, plus how the shared glue labels and logs it (see
    /// [`write_target`]).
    hypridle_conf: StoreBackedFile,
}

impl PowerModel {
    /// Builds the model, recording the `hypridle.conf` path (the production entry point,
    /// called from the startup worker — architecture §8).
    pub fn load(hypridle_conf: PathBuf) -> PowerModel {
        PowerModel {
            hypridle_conf: write_target(hypridle_conf),
        }
    }

    /// The Power & Idle page's contribution to an Apply: the store's dirty timeouts and
    /// lock command rendered into one surgical `hypridle.conf` [`FileWrite`] (task 6.8;
    /// named like every other page's Apply seam since task 9.17).
    ///
    /// `dirty` is the store's dirty Power & Idle settings (from
    /// [`SettingsStore::dirty_in_category`](crate::core::store::SettingsStore::dirty_in_category)).
    /// The shared glue ([`StoreBackedFile::render_write`]) reads the current file bytes and
    /// applies them through [`render_hypridle_conf`]; see it for the three answers this can
    /// give — no write, one write, or an error the caller must turn into an aborted Apply
    /// rather than a skipped file. Both failure modes are near-unreachable in practice
    /// (`hypridle.conf` is readable and was parseable at load).
    ///
    /// The "no write" answer is also what makes the hypridle restart fire **only** when the
    /// file actually changed: with no write, the pipeline derives no `HypridleConf` change
    /// and plans no restart.
    pub fn apply_contribution(
        &self,
        dirty: &[(SettingId, Value)],
    ) -> Result<Option<FileWrite>, StoreWriteError<HypridleRenderError>> {
        self.hypridle_conf.render_write(dirty, render_hypridle_conf)
    }
}

/// How the shared store-backed write glue addresses, labels and logs `hypridle.conf`
/// (task 9.17).
///
/// Only the path varies between runs, so everything else the glue needs is fixed here, at
/// the one place a reader can check it against the file it describes.
fn write_target(path: PathBuf) -> StoreBackedFile {
    StoreBackedFile {
        path,
        name: "hypridle.conf",
        backing: BackingFile::HypridleConf,
        // Rendered from the store's dirty Power & Idle settings, which the plan carries as
        // its validations (both come from the same `dirty_ids()`).
        validation: WriteValidation::InPlan,
        read_failure_log: "could not read hypridle.conf to render the Power & Idle edits; \
                           aborting the apply (R8.3)",
        render_failure_log: "failed to render a hypridle.conf edit; aborting the apply (R8.3)",
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
    use crate::testing::differing_lines;

    /// A realistic `hypridle.conf` fixture in the real dotfiles' shape (analysis §6, §7):
    /// a `general { }` block with the lock command (carrying hyprlock's `--grace 10`),
    /// then three `listener { }` blocks in the dim / lock / DPMS order, each carrying an
    /// inline comment so comment preservation is exercised.
    const HYPRIDLE_CONF: &str = "\
general {
    lock_cmd = pidof hyprlock || hyprlock --grace 10
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
        assert_eq!(power_key_path(SettingId::MouseSensitivity), None);
    }

    #[test]
    fn a_listener_timeout_edit_leaves_other_blocks_intact() {
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
            .position(|l| l == "    lock_cmd = pidof hyprlock || hyprlock --grace 10")
            .expect("fixture has the lock command");
        assert_eq!(differing_lines(HYPRIDLE_CONF, &edited), vec![target]);
        assert_eq!(
            edited.lines().nth(target),
            Some("    lock_cmd = hyprlock --immediate")
        );
    }

    /// A `hypridle.conf` whose lock command has no grace flag and a trailing comment,
    /// for the insert / remove byte-diff tests (the real file's `lock_cmd` line carries
    /// no comment, so this proves the hyprlang writer keeps one beside a token edit).
    const COMMENTED_LOCK_CMD: &str = "\
general {
    lock_cmd = pidof hyprlock || hyprlock    # lock unless already locked
    before_sleep_cmd = loginctl lock-session
}

listener {
    timeout = 300                                # 5min
    on-timeout = loginctl lock-session           # lock the session.
}
";

    /// The one line of `original` that changed in `edited`, asserting nothing else did.
    fn single_changed_line(original: &str, edited: &str) -> String {
        let changed = differing_lines(original, edited);
        assert_eq!(changed.len(), 1, "exactly one line may change: {changed:?}");
        edited
            .lines()
            .nth(changed[0])
            .expect("the changed line exists")
            .to_string()
    }

    #[test]
    fn the_grace_period_shares_the_lock_command_address() {
        assert_eq!(
            power_key_path(SettingId::LockGracePeriod).map(|p| p.to_string()),
            Some("general.lock_cmd".to_string())
        );
    }

    #[test]
    fn a_grace_edit_changes_only_the_number_on_the_lock_cmd_line() {
        // Change (task 11.13): `--grace 10` -> `--grace 30`; the `pidof hyprlock ||`
        // guard and every other line stay byte-identical.
        let edit = render_hypridle_conf(
            HYPRIDLE_CONF.as_bytes(),
            &[(SettingId::LockGracePeriod, Value::Integer(30))],
        )
        .expect("a grace edit renders");
        let edited = String::from_utf8(edit.contents).expect("valid utf-8");
        assert_eq!(
            single_changed_line(HYPRIDLE_CONF, &edited),
            "    lock_cmd = pidof hyprlock || hyprlock --grace 30"
        );
        assert_eq!(edit.changed_keys, vec!["general.lock_cmd".to_string()]);
    }

    #[test]
    fn a_grace_edit_inserts_the_flag_keeping_the_comment() {
        // Insert: no flag yet, so ` --grace 15` goes right after the `hyprlock` word;
        // the alignment spaces and the `#` comment after the value survive.
        let edited = String::from_utf8(
            render_hypridle_conf(
                COMMENTED_LOCK_CMD.as_bytes(),
                &[(SettingId::LockGracePeriod, Value::Integer(15))],
            )
            .expect("a grace insert renders")
            .contents,
        )
        .expect("valid utf-8");
        assert_eq!(
            single_changed_line(COMMENTED_LOCK_CMD, &edited),
            "    lock_cmd = pidof hyprlock || hyprlock --grace 15    # lock unless already locked"
        );
    }

    #[test]
    fn a_zero_grace_removes_the_flag_keeping_the_comment() {
        // Remove: grace 0 is hyprlock's default, so the token goes instead of reading
        // `--grace 0`; the rest of the line, comment included, is unchanged.
        let with_grace =
            COMMENTED_LOCK_CMD.replace("|| hyprlock    #", "|| hyprlock --grace 10    #");
        let edited = String::from_utf8(
            render_hypridle_conf(
                with_grace.as_bytes(),
                &[(SettingId::LockGracePeriod, Value::Integer(0))],
            )
            .expect("a grace removal renders")
            .contents,
        )
        .expect("valid utf-8");
        assert_eq!(
            edited, COMMENTED_LOCK_CMD,
            "removal restores the flagless line"
        );
    }

    #[test]
    fn a_lock_command_and_grace_edit_apply_the_command_first() {
        // The interaction rule: the staged lock command is written, then the grace
        // token is set on that new value — and the one line is reported once.
        let edit = render_hypridle_conf(
            HYPRIDLE_CONF.as_bytes(),
            &[
                (
                    SettingId::LockCommand,
                    Value::String("hyprlock --no-fade-in".to_string()),
                ),
                (SettingId::LockGracePeriod, Value::Integer(20)),
            ],
        )
        .expect("the combined edit renders");
        let edited = String::from_utf8(edit.contents).expect("valid utf-8");
        assert_eq!(
            single_changed_line(HYPRIDLE_CONF, &edited),
            "    lock_cmd = hyprlock --grace 20 --no-fade-in"
        );
        assert_eq!(edit.changed_keys, vec!["general.lock_cmd".to_string()]);

        // The order of the batch does not matter: the grace token always goes last.
        let reversed = render_hypridle_conf(
            HYPRIDLE_CONF.as_bytes(),
            &[
                (SettingId::LockGracePeriod, Value::Integer(20)),
                (
                    SettingId::LockCommand,
                    Value::String("hyprlock --no-fade-in".to_string()),
                ),
            ],
        )
        .expect("the reversed batch renders");
        assert_eq!(reversed.contents, edited.as_bytes());
    }

    #[test]
    fn a_grace_already_in_place_reports_no_change() {
        // The store stages both views together, so the lock command usually already
        // carries the staged grace; the grace step then adds no second changed key, and
        // a grace-only batch with nothing to change yields no changed key at all (so the
        // shared glue writes nothing and no restart is planned).
        let edit = render_hypridle_conf(
            HYPRIDLE_CONF.as_bytes(),
            &[(SettingId::LockGracePeriod, Value::Integer(10))],
        )
        .expect("renders");
        assert_eq!(edit.contents, HYPRIDLE_CONF.as_bytes());
        assert!(edit.changed_keys.is_empty());
    }

    #[test]
    fn a_grace_for_a_command_without_hyprlock_is_refused() {
        // Backstop for a lock command changed on disk since the grace was staged: the
        // Apply aborts rather than guess where a grace period would go.
        const SWAYLOCK: &str = "general {\n    lock_cmd = swaylock -f\n}\n";
        assert_eq!(
            render_hypridle_conf(
                SWAYLOCK.as_bytes(),
                &[(SettingId::LockGracePeriod, Value::Integer(5))],
            )
            .map(|edit| edit.contents),
            Err(HypridleRenderError::Grace(GraceError::NoHyprlock))
        );
        // No lock command at all.
        assert_eq!(
            render_hypridle_conf(
                b"listener {\n    timeout = 1\n}\n",
                &[(SettingId::LockGracePeriod, Value::Integer(5))],
            )
            .map(|edit| edit.contents),
            Err(HypridleRenderError::MissingLockCommand)
        );
        // A negative grace never reaches the token editor.
        assert!(matches!(
            render_hypridle_conf(
                HYPRIDLE_CONF.as_bytes(),
                &[(SettingId::LockGracePeriod, Value::Integer(-1))],
            ),
            Err(HypridleRenderError::GraceValue(Value::Integer(-1)))
        ));
    }

    #[test]
    fn grace_render_errors_read_as_plain_sentences() {
        // These messages are quoted verbatim in the Apply-failure dialog, so pin them
        // exactly: no stray whitespace from a broken line continuation, and the grace
        // number shown as a number rather than a debug-formatted `Value`.
        assert_eq!(
            HypridleRenderError::MissingLockCommand.to_string(),
            "the grace period cannot be written: there is no `lock_cmd` in the `general` section"
        );
        assert_eq!(
            HypridleRenderError::GraceValue(Value::Integer(-1)).to_string(),
            "-1 is not a whole, non-negative number of seconds for the grace period"
        );
        assert_eq!(
            HypridleRenderError::GraceValue(Value::Bool(true)).to_string(),
            "a boolean value cannot be a grace period, which is a whole number of seconds"
        );
        assert_eq!(
            HypridleRenderError::Grace(GraceError::NoHyprlock).to_string(),
            "the grace period cannot be written: the lock command does not start hyprlock"
        );
    }

    #[test]
    fn a_full_edit_changes_only_the_touched_lines() {
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
        assert!(matches!(
            result,
            Err(HypridleRenderError::Edit(EditError::SectionNotFound(_)))
        ));
    }

    #[test]
    fn apply_contribution_is_none_when_clean() {
        // No dirty Power & Idle settings -> Ok(None), no write (the common clean case).
        // This is also what makes the restart fire only on a real change: no write means
        // no HypridleConf in the reload set.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hypridle.conf");
        fs::write(&path, HYPRIDLE_CONF).expect("write hypridle.conf");
        let model = PowerModel::load(path);
        assert!(matches!(model.apply_contribution(&[]), Ok(None)));
    }

    #[test]
    fn a_dirty_edit_on_an_unreadable_file_is_an_error() {
        // A dirty Power & Idle edit against a missing file is an error (the Apply aborts),
        // never a silent skip that would let commit_apply promote an unwritten value.
        let dir = tempfile::tempdir().expect("temp dir");
        let model = PowerModel::load(dir.path().join("gone.conf"));
        let dirty = vec![(SettingId::DimTimeout, Value::Integer(90))];
        let error = model
            .apply_contribution(&dirty)
            .expect_err("a missing hypridle.conf must fail");
        assert!(matches!(error, StoreWriteError::Read { .. }));
        // The dialog quotes this message verbatim, so it has to name the file the user has
        // to fix (the `name` the shared glue is given — task 9.17).
        assert!(
            error
                .to_string()
                .starts_with("hypridle.conf could not be read: "),
            "the message names hypridle.conf: {error}"
        );
    }

    #[test]
    fn the_writer_rejects_an_unsafe_lock_command() {
        // Defense in depth: even though the stage-time validator rejects a lock
        // command containing `#`/newline before it can be staged, the writer keeps its own
        // `reject_unsafe_value` guard — a lock command with a `#` surfaces as a
        // `StoreWriteError::Render` here, so a value that somehow reached this point
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
            model.apply_contribution(&dirty),
            Err(StoreWriteError::Render {
                error: HypridleRenderError::Edit(EditError::InvalidValue(_)),
                ..
            })
        ));
    }

    #[test]
    fn a_power_edit_applies_with_a_hypridle_restart() {
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
            .apply_contribution(&dirty)
            .expect("no error rendering the write")
            .expect("a dirty Power & Idle setting produces a write");
        assert_eq!(write.path, path);
        assert_eq!(write.backing, BackingFile::HypridleConf);
        // Task 9.10's drift guard trusts this declaration rather than checking it, so a
        // page silently claiming NotNeeded would disable the guard for its own write
        // (task 9.17 review). Pinned here, where the claim is made.
        assert_eq!(write.validation, WriteValidation::InPlan);
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
            links: Vec::new(),
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
                ..
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
        assert!(matches!(model.apply_contribution(&[]), Ok(None)));

        // A plan with no writes (the clean Power & Idle case) reloads nothing.
        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("baseline hypridle.conf");
        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: Vec::new(),
            links: Vec::new(),
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
    fn a_second_apply_after_commit_is_not_a_self_conflict() {
        // Mirrors the Input page's end-to-end self-conflict test (task 6.6):
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
                        Value::String("pidof hyprlock || hyprlock --grace 10".to_string()),
                    ),
                    (SettingId::LockGracePeriod, Value::Integer(10)),
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
            .apply_contribution(&dirty)
            .expect("no error")
            .expect("a write");
        let written_bytes = write.contents.clone();
        let plan = ApplyPlan {
            validations: dirty,
            writes: vec![write],
            links: Vec::new(),
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
            .apply_contribution(&dirty2)
            .expect("no error")
            .expect("a write");
        let plan2 = ApplyPlan {
            validations: dirty2,
            writes: vec![write2],
            links: Vec::new(),
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
