//! GTK-free Display-page domain model (tasks 6.1, 11.8, 11.9; architecture §6
//! "Staging" + the Display page; R2.3, R4.2, R4.4, R5.2, R5.4, R8.3, R8.5, R6.2).
//!
//! # What this module is
//!
//! The Display page edits per-monitor resolution/refresh/scale/position (staged to
//! `config/hypr/monitors.conf`, applied on **Apply**) and offers a runtime-only
//! laptop-display toggle (applied immediately, R5.2). Unlike every other setting in
//! the app, monitors are **known only at runtime** — their names, count, and the
//! modes they support come from `hyprctl monitors all -j` — so they do not fit the
//! fixed, fieldless [`SettingId`](crate::core::model::SettingId) enum the
//! [`SettingsStore`](crate::core::store) is keyed by.
//!
//! # The per-monitor identity decision (design note)
//!
//! Rather than making [`SettingId`](crate::core::model::SettingId) carry a monitor
//! name (which would ripple a non-`Copy`, dynamic key through the whole store), this
//! module is a **self-contained, Display-page-local staging model** ([`DisplayModel`])
//! that mirrors the store's `original`/`staged`/dirty pattern but keyed by monitor.
//! It owns the parsed `monitors.conf`, tracks a staged [`MonitorConfig`] per monitor,
//! and produces the pieces the shared Apply pipeline needs — a [`FileWrite`] for
//! `monitors.conf` (rewriting only the changed records via the surgical
//! [`monitors`](crate::parsers::monitors) parser) plus the value validations. The
//! window merges that contribution into the same [`apply::run`](crate::core::apply)
//! it already drives for the store, so this page reuses the existing
//! write/rollback/reload machinery without bending the store around dynamic keys. It
//! stays GTK-free so the merge and the write production are unit-tested headlessly
//! (R6.2); the layering guard in `tests/module_boundaries.rs` forbids any
//! `gtk`/`relm4` import here.
//!
//! # The merge: `monitors.conf` records + `hyprctl monitors all -j`
//!
//! [`DisplayModel::from_sources`] merges two views of each output (R2.3):
//!
//! - the **live** state from `hyprctl monitors all -j` (`all` so a disabled output
//!   is still listed, with `"disabled": true`) — which outputs exist, the modes
//!   they *support* (for the resolution/refresh drop-downs), and their current
//!   mode/scale/position/enabled state;
//! - the **configured** values from the matching `monitor=` record — the drop-downs'
//!   current selections. A record is matched to a live output by exact name
//!   (`eDP-1`) or a `desc:` substring, honouring Hyprland's later-rule-wins order.
//!
//! An output with no specific record is still shown (configured by the catch-all or
//! defaults); editing it appends a specific record. When `monitors.conf` cannot be
//! read at all (R4.4), or breaks its frozen grammar (see below), the file-backed
//! drop-downs are hidden ([`DisplayModel::records_editable`] is `false`) but the
//! runtime laptop toggle still works.
//!
//! # Keeping `monitors.conf` inside its frozen grammar (CRITICAL; task 11.9, R8.3)
//!
//! Hyprland no longer reads `monitors.conf` itself: the monitor loader in
//! `hyprland.lua` turns each record into an `hl.monitor({ … })` call, and
//! `scripts/hypr-display-profile.sh` `awk`-parses the laptop panel's record (the
//! single source for its mode and scale, analysis §6.2). Both rely on the file's
//! **frozen grammar** `monitor=<output>,<mode>,<position>,<scale>[,<key>,<value>…]`
//! (analysis §7.2), which the [`monitors`](crate::parsers::monitors) parser
//! enforces as named [`GrammarRule`]s. This model applies it at three points:
//!
//! - **at load** — a `monitors.conf` that already breaks the grammar (a hand
//!   edit) is logged at `warn` and makes the page **read-only**
//!   ([`DisplayModel::records_editable`] is `false`, [`DisplayModel::records_problem`]
//!   names the line and rule). The app never "fixes" a record it did not write,
//!   and every write re-validates the whole file, so editing *around* a broken
//!   line could never be applied anyway; refusing up front is the same graceful
//!   degradation as an unreadable file (R4.4), with the reason shown;
//! - **at stage time** — every `stage_*` call renders the edit into a copy of the
//!   file and validates the *whole result* (the touched record, its untouched
//!   extras, and the file-level rules), refusing the stage with a
//!   [`StageRefusal`] naming the rule (R8.3), in addition to the per-value
//!   validators for mode, position and scale;
//! - **before the write** — [`DisplayModel::apply_contribution`] validates the
//!   emitted bytes once more, and the parser's edit methods refuse any edit that
//!   leaves a broken record, as the last line of defence.
//!
//! Edits go through the parser surgically: an in-place field edit
//! ([`set_field`](crate::parsers::monitors::MonitorsFile::set_field)) keeps the
//! record's other fields and extras byte-identical, a (re-)enable or first
//! configure writes a whole record
//! ([`set_record`](crate::parsers::monitors::MonitorsFile::set_record)). The app
//! never touches either downstream script.
//!
//! **Disabling a non-laptop output is refused.** The only on-disk "off" form the
//! app used to write, `monitor=<output>,disable`, is outside the grammar: the Lua
//! loader would pass `disable` to Hyprland as a *mode*, which Hyprland rejects
//! with a config error while leaving the output on ([`GrammarRule::DisableForm`]).
//! So [`DisplayModel::stage_enabled`] refuses `false` with that rule named rather
//! than write a record that does not do what it says. Enabling (writing a
//! complete record for an output that is off) still works.
//!
//! # The laptop-display toggle runs the dotfiles script (R5.2; task 11.8)
//!
//! The internal panel's on/off state is **not** a `monitor=…,disable` record — the
//! hotplug watcher (`scripts/hypr-monitor-hotplug`) and the Lua monitor loader in
//! `hyprland.lua` own the docked state, and a record would fight them (analysis
//! §7.3, §8.1). The dotfiles ship a script for exactly this switch,
//! `scripts/hypr-toggle-laptop-display`: it issues the live `hl.monitor({ … })`
//! call, creates or removes the `/tmp/hypr-laptop-display-forced` override the
//! watcher and the loader read, sends a desktop notification and relaunches the eww
//! bars. The app runs that script — found under the dotfiles repo root by detection
//! (R8.5), and the switch is hidden when it is absent (R4.2) — instead of
//! re-implementing any of its steps, so it never writes the state file, never
//! launches bars and never issues a monitor call itself.
//!
//! The script is a **pure toggle** (it takes no arguments and flips whatever state
//! it finds), so the app brackets it with two reads of the compositor's view and
//! renders the switch from the *read* state, never from an assumed one
//! ([`DisplayModel::toggle_laptop`]). The toggle never stages, never becomes dirty,
//! and never touches `monitors.conf`. The panel's *mode and scale*, by contrast,
//! live in the eDP `monitor=` record and are edited like any other monitor (the
//! single-source gotcha) — only its enablement is runtime.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::core::apply::{FileWrite, WriteMode, WriteValidation, stderr_excerpt};
use crate::core::freshness::FreshnessTracker;
use crate::core::model::{
    SCALE_RANGE, SettingId, ValidationError, Value, validate_float_range, validate_monitor_mode,
    validate_monitor_position,
};
use crate::core::reload::BackingFile;
use crate::parsers::monitors::{
    EditError, GrammarRule, GrammarViolation, MonitorField, MonitorRecord, MonitorsFile,
    check_value,
};
use crate::system::command::{Command, CommandError, CommandRunner};

/// The output the dotfiles' toggle script switches. `scripts/hypr-toggle-laptop-display`
/// hard-codes `eDP-1` (analysis §7.3), so the app offers the switch only on the
/// output of that name: on a panel named anything else the script would flip a
/// different output than the one the switch is drawn under.
const TOGGLE_SCRIPT_OUTPUT: &str = "eDP-1";

/// Output-name prefix identifying an internal laptop panel (`eDP-1`, `eDP-2`, …),
/// matched case-insensitively. Such a monitor's enablement uses the runtime toggle
/// (R5.2) rather than a staged `monitors.conf` edit; its mode/scale are still edited
/// through its record (the single-source gotcha, analysis §6.2).
const LAPTOP_OUTPUT_PREFIX: &str = "edp";

/// Curated scale factors offered in the scale drop-down, in ascending order. The
/// monitor's currently-configured scale is always added too (see
/// [`MonitorView::scale_options`]) so an unusual on-disk value stays selectable.
/// These cover the common integer and fractional scales in use (analysis §4/§6.2:
/// `1`, `1.066667`, `1.333333`).
const CURATED_SCALES: &[&str] = &["1", "1.25", "1.333333", "1.5", "1.75", "2"];

/// Position choices offered in the position drop-down. `auto` lets Hyprland place the
/// output; `0x0` anchors it at the origin. The configured value is always added too.
/// Fine multi-monitor layout (arbitrary coordinates) is out of v1 scope — a user who
/// needs it edits `monitors.conf` directly — so the drop-down stays a small, safe set.
const CURATED_POSITIONS: &[&str] = &["auto", "0x0"];

/// One mode a monitor supports, split from an `hyprctl` `availableModes` entry such
/// as `2560x1440@60.01Hz`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mode {
    /// The resolution part, `WIDTHxHEIGHT` (e.g. `2560x1440`).
    resolution: String,
    /// The refresh rate as a cleaned string (trailing zeros trimmed), e.g. `60.01`
    /// or `120`. This is what a chosen mode writes after the `@`.
    refresh: String,
}

/// A monitor's editable configuration: the values shown in (and written from) the
/// per-monitor drop-downs.
///
/// Derives [`PartialEq`] so a staged config can be compared to the original for
/// dirty tracking. `mode` is the full `monitor=` mode field (`WIDTHxHEIGHT@REFRESH`,
/// a bare `WIDTHxHEIGHT`, or a token like `preferred`); `position` and `scale` are the
/// record's position and scale fields.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MonitorConfig {
    /// Whether the output is enabled. For a laptop panel this mirrors the record and
    /// is never staged (its live enablement is the runtime toggle); for other outputs
    /// toggling it stages a `monitors.conf` enable/disable edit.
    enabled: bool,
    /// The mode field (`WIDTHxHEIGHT@REFRESH`, a bare `WIDTHxHEIGHT`, or `preferred`).
    mode: String,
    /// The position field (`XxY` or `auto`).
    position: String,
    /// The scale field (e.g. `1.333333`).
    scale: String,
}

/// One monitor as presented on the Display page: its live identity and supported
/// modes merged with its configured values and any pending staged edit.
#[derive(Clone, Debug)]
struct Monitor {
    /// The live output name from `hyprctl` (`eDP-1`), shown in the UI and used to
    /// detect the laptop panel.
    output_name: String,
    /// The `hyprctl` description (make/model), shown as a secondary label.
    description: String,
    /// Whether this is an internal laptop panel (its enablement is the runtime
    /// toggle, R5.2). Its mode/scale are still edited via the record.
    is_laptop: bool,
    /// Whether the compositor last reported this output as enabled (`!disabled` in
    /// `hyprctl monitors all -j`). For a laptop panel this — not the record — drives
    /// the runtime enable switch's position (the panel's live on/off state). It is
    /// only ever set from a read: at load, and by [`DisplayModel::toggle_laptop`]
    /// from the reads around the toggle script, never from the state a toggle was
    /// expected to produce.
    live_enabled: bool,
    /// The `monitor=` record name that specifically configures this output (an exact
    /// name or a matched `desc:` rule), or `None` when only the catch-all/defaults
    /// apply. Edits address the record by this name; when `None`, the output name is
    /// used to append a new specific record.
    record_name: Option<String>,
    /// Whether a specific record for this output exists **and is enabled** — the
    /// condition under which an in-place [`set_field`](crate::parsers::monitors::MonitorsFile::set_field)
    /// edit applies; otherwise a full record is written with
    /// [`set_record`](crate::parsers::monitors::MonitorsFile::set_record).
    has_enabled_record: bool,
    /// The modes the compositor reports this output supports (for the drop-downs).
    available: Vec<Mode>,
    /// The configured values as read (the drop-downs' baseline selections).
    original: MonitorConfig,
    /// The pending staged edit, or `None` when unedited (mirrors the store's model).
    staged: Option<MonitorConfig>,
}

impl Monitor {
    /// The record name to address in `monitors.conf` for this output: the specific
    /// record if one exists, otherwise the live output name (used to append a new
    /// record).
    fn address(&self) -> &str {
        self.record_name.as_deref().unwrap_or(&self.output_name)
    }

    /// The values the UI should show: the staged edit if present, else the original.
    fn effective(&self) -> &MonitorConfig {
        self.staged.as_ref().unwrap_or(&self.original)
    }

    /// Whether this monitor has a pending edit differing from its original.
    fn is_dirty(&self) -> bool {
        self.staged
            .as_ref()
            .is_some_and(|staged| staged != &self.original)
    }
}

/// The Display page's staging model: the merged monitors, the parsed `monitors.conf`,
/// and the dotfiles script behind the runtime laptop toggle (tasks 6.1, 11.8).
///
/// See the module docs for the design. Built by [`Self::load`] (production) or
/// [`Self::from_sources`] (tests, with canned `hyprctl` JSON). Its file-backed edits
/// are surfaced to the Apply pipeline through [`Self::apply_contribution`]; its
/// runtime laptop toggle is [`Self::toggle_laptop`].
pub struct DisplayModel {
    /// The merged monitors, in the order `hyprctl` reported them.
    monitors: Vec<Monitor>,
    /// The parsed `monitors.conf`, or `None` when it could not be read (R4.4) — in
    /// which case the file-backed controls are hidden but the laptop toggle still
    /// works.
    records: Option<MonitorsFile>,
    /// The first frozen-grammar rule the loaded `monitors.conf` breaks, or `None`
    /// when it follows the grammar (task 11.9). `Some` makes the page read-only:
    /// staging is refused and the file-backed controls are hidden (see the module
    /// docs for why the app does not edit around a broken record).
    records_problem: Option<GrammarViolation>,
    /// The live XDG path of `monitors.conf`, the target of the produced [`FileWrite`]
    /// (R8.5). The [atomic writer](crate::system::writer) canonicalizes it, so a
    /// symlink into a dotfiles repo has its real target rewritten and the link
    /// preserved.
    monitors_conf_path: PathBuf,
    /// The dotfiles' `scripts/hypr-toggle-laptop-display`, as found by detection
    /// ([`Capabilities::laptop_toggle_script`](crate::core::detect::Capabilities::laptop_toggle_script)),
    /// or `None` when it is absent — which hides the laptop toggle (R4.2) while the
    /// rest of the page stays.
    laptop_toggle_script: Option<PathBuf>,
    /// The freshness baseline for `monitors.conf`, recorded from the exact bytes read
    /// at load (R5.6). [`Self::apply_contribution`]'s caller conflict-checks against it
    /// before writing so an external edit is not clobbered, and [`Self::commit`]
    /// re-baselines it from the bytes just written so the app's own write is not
    /// mistaken for an external change on the next Apply. Empty when `monitors.conf`
    /// was unreadable (nothing to write, so nothing to check).
    freshness: FreshnessTracker,
}

/// Why [`DisplayModel::apply_contribution`] could not produce a write despite there
/// being dirty monitor edits to apply (task 9.5).
///
/// This is distinct from "nothing was dirty" (which is a plain `Ok(None)`): when the
/// user *has* pending monitor edits but the write cannot be rendered, the Apply must
/// **abort** rather than skip the write and let the model [`commit`](DisplayModel::commit)
/// the staged values — that would promote them and re-baseline `monitors.conf`'s
/// freshness against bytes no write ever produced, desyncing the model from disk. The
/// store-backed pages (whose shared glue is
/// [`crate::core::store_write`]) return an error in exactly this case for the same reason.
///
/// Unlike those siblings there is no read-failure variant: this model holds
/// `monitors.conf` parsed from the startup load rather than re-reading it per Apply, and
/// a file that was unreadable *then* leaves the page in its documented degraded state
/// (R4.4 — the page hides the file-backed rows, so nothing can be staged from the UI),
/// which stays a plain `Ok(None)`.
#[derive(Debug)]
pub enum DisplayWriteError {
    /// The `monitors.conf` writer rejected an edit — a value or record that would
    /// break the frozen grammar ([`EditError::Grammar`]), or a record shape it cannot
    /// edit in place.
    Render(EditError),
    /// The rendered file breaks the frozen grammar (analysis §7.2) somewhere — the
    /// whole-file re-validation of the bytes about to be written, the last line of
    /// defence of R8.3 "validate all before any write".
    Grammar(GrammarViolation),
}

impl fmt::Display for DisplayWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisplayWriteError::Render(error) => {
                write!(f, "the monitors.conf edit could not be applied: {error}")
            }
            DisplayWriteError::Grammar(violation) => write!(
                f,
                "the monitors.conf edit was refused because the result would break its \
                 frozen format at {violation}"
            ),
        }
    }
}

impl std::error::Error for DisplayWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DisplayWriteError::Render(error) => Some(error),
            DisplayWriteError::Grammar(violation) => Some(violation),
        }
    }
}

/// Why a `stage_*` call on [`DisplayModel`] refused an edit (R8.3; task 11.9).
///
/// A refused edit leaves the model exactly as it was — nothing is staged, so the
/// page's control snaps back when it re-renders — and the page shows this
/// reason under the monitor, so the user learns which rule the value broke
/// instead of seeing the control silently revert.
#[derive(Debug)]
pub enum StageRefusal {
    /// A mode, position or scale value failed the typed settings validators
    /// ([`validate_monitor_mode`], [`validate_monitor_position`], the
    /// [`SCALE_RANGE`] check).
    Value(ValidationError),
    /// The scale is not a number at all.
    ScaleNotANumber(String),
    /// `monitors.conf` could not be read at load, so there is no record to edit
    /// (R4.4).
    RecordsUnavailable,
    /// The loaded `monitors.conf` already breaks the frozen grammar, so the page is
    /// read-only until the file is fixed (see the module docs).
    FileBreaksGrammar(GrammarViolation),
    /// The edit would produce a `monitors.conf` that breaks the frozen grammar
    /// (analysis §7.2) — the named rule. This is the whole-record check: it covers
    /// the record's untouched fields and the file-level rules, not only the value
    /// that changed.
    Grammar(GrammarRule),
    /// The writer could not render the edit for a reason other than the grammar.
    /// Not reachable from a grammar-clean file today (every record then has the
    /// fields an edit addresses), kept so a refusal never panics.
    Render(EditError),
    /// The output is a laptop panel, whose on/off state is the runtime toggle
    /// ([`DisplayModel::toggle_laptop`], R5.2), never a staged record edit.
    LaptopEnablement,
    /// The index names no monitor (task 9.24): it was taken before the model was
    /// rebuilt with fewer monitors (a Refresh or a conflict reload). Refusing —
    /// rather than panicking, or silently reporting success — keeps the app
    /// running and tells the caller that nothing was staged (R8.3).
    UnknownMonitor {
        /// The index that was asked for.
        index: usize,
    },
}

impl fmt::Display for StageRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageRefusal::Value(error) => write!(f, "{error}"),
            StageRefusal::ScaleNotANumber(scale) => {
                write!(f, "the scale `{scale}` is not a number")
            }
            StageRefusal::RecordsUnavailable => write!(
                f,
                "monitors.conf could not be read, so there is no record to edit"
            ),
            StageRefusal::FileBreaksGrammar(violation) => write!(
                f,
                "monitors.conf is read-only because it breaks its frozen format at \
                 {violation}"
            ),
            StageRefusal::Grammar(rule) => {
                write!(f, "monitors.conf format: {rule}")
            }
            StageRefusal::Render(error) => write!(f, "{error}"),
            StageRefusal::LaptopEnablement => write!(
                f,
                "the laptop panel is switched on and off with its runtime toggle, not a \
                 monitors.conf record"
            ),
            StageRefusal::UnknownMonitor { index } => write!(
                f,
                "monitor {index} is no longer listed; the page is out of date"
            ),
        }
    }
}

impl std::error::Error for StageRefusal {}

impl From<DisplayWriteError> for StageRefusal {
    /// A trial render's failure, as the stage refusal that names it: a grammar
    /// rule (from either the writer's own check or the whole-file check) becomes
    /// [`StageRefusal::Grammar`], any other writer error [`StageRefusal::Render`].
    fn from(error: DisplayWriteError) -> Self {
        match error {
            DisplayWriteError::Render(EditError::Grammar(rule)) => StageRefusal::Grammar(rule),
            DisplayWriteError::Render(error) => StageRefusal::Render(error),
            DisplayWriteError::Grammar(violation) => {
                StageRefusal::Grammar(violation.rule().clone())
            }
        }
    }
}

/// Why [`DisplayModel::toggle_laptop`] did not leave the laptop panel in the
/// requested state (task 11.8).
///
/// Every variant is non-fatal (R5.5): the page shows the message next to the switch,
/// which is re-drawn from the last *read* panel state. The script's own failure, when
/// there was one, rides along — it explains a failed switch, but on its own does not
/// make a switch fail (see [`DisplayModel::toggle_laptop`]).
#[derive(Debug)]
pub enum LaptopToggleError {
    /// The toggle is not offered for this monitor: detection found no toggle script,
    /// the monitor is not the output the script switches, or the index names no
    /// monitor at all (an index left over from before a model rebuild). The page
    /// hides the switch in the first two cases, so this is reached only by a caller
    /// that skipped [`MonitorView::laptop_toggle_available`] or held a stale index.
    Unavailable,
    /// The state could not be read *before* the script, so the script was not run:
    /// without that read a pure toggle could flip the panel the wrong way.
    ReadBefore(PanelReadError),
    /// The script ran, but the state could not be read afterwards, so whether the
    /// panel switched is unknown; the switch keeps showing the state read before.
    ReadAfter {
        /// Why the second read failed.
        error: PanelReadError,
        /// The script's own failure, or `None` when it reported success.
        script: Option<ToggleScriptFailure>,
    },
    /// The script ran, but the panel is still in the state it was in before — the
    /// script failed, or something else (the hotplug watcher) switched it back.
    Unchanged {
        /// The panel's state, read after the script (unchanged from before).
        enabled: bool,
        /// The script's own failure, or `None` when it reported success.
        script: Option<ToggleScriptFailure>,
    },
}

impl fmt::Display for LaptopToggleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaptopToggleError::Unavailable => {
                f.write_str("the laptop-display toggle script is not available for this panel")
            }
            LaptopToggleError::ReadBefore(error) => write!(
                f,
                "the panel's current state could not be read, so the toggle script was not \
                 run: {error}"
            ),
            LaptopToggleError::ReadAfter { error, script } => {
                write!(
                    f,
                    "the toggle script ran, but the panel's new state could not be read: {error}"
                )?;
                write_script_failure(f, script.as_ref())
            }
            LaptopToggleError::Unchanged { enabled, script } => {
                let state = if *enabled { "on" } else { "off" };
                write!(
                    f,
                    "the panel is still {state} after running the toggle script"
                )?;
                write_script_failure(f, script.as_ref())
            }
        }
    }
}

impl std::error::Error for LaptopToggleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LaptopToggleError::Unavailable => None,
            LaptopToggleError::ReadBefore(error) | LaptopToggleError::ReadAfter { error, .. } => {
                Some(error)
            }
            LaptopToggleError::Unchanged { script, .. } => {
                script.as_ref().map(|failure| failure as _)
            }
        }
    }
}

/// Appends `; <script failure>` to a [`LaptopToggleError`] message when the script
/// itself reported one.
fn write_script_failure(
    f: &mut fmt::Formatter<'_>,
    script: Option<&ToggleScriptFailure>,
) -> fmt::Result {
    match script {
        Some(failure) => write!(f, "; {failure}"),
        None => Ok(()),
    }
}

/// Why the laptop panel's state could not be read from `hyprctl monitors all -j`
/// around a toggle (task 11.8).
#[derive(Debug)]
pub enum PanelReadError {
    /// `hyprctl` could not be run at all (not found, timed out).
    Command(CommandError),
    /// `hyprctl` ran and reported failure — typically the compositor is gone.
    Failed {
        /// The exit code, or `None` when the process was terminated by a signal.
        code: Option<i32>,
    },
    /// The reply does not list the panel (or is not the expected JSON), so its
    /// state is unknown.
    OutputMissing {
        /// The output that was looked for (`eDP-1`).
        output: String,
    },
}

impl fmt::Display for PanelReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PanelReadError::Command(error) => write!(f, "could not run hyprctl: {error}"),
            PanelReadError::Failed { code: Some(code) } => {
                write!(f, "hyprctl monitors exited with status {code}")
            }
            PanelReadError::Failed { code: None } => {
                f.write_str("hyprctl monitors was terminated by a signal")
            }
            PanelReadError::OutputMissing { output } => {
                write!(f, "hyprctl does not list the output {output}")
            }
        }
    }
}

impl std::error::Error for PanelReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PanelReadError::Command(error) => Some(error),
            PanelReadError::Failed { .. } | PanelReadError::OutputMissing { .. } => None,
        }
    }
}

/// How the dotfiles' laptop-display toggle script failed (task 11.8). Carried by a
/// [`LaptopToggleError`] to explain a failed switch, and logged at `warn` when the
/// panel switched despite it.
#[derive(Debug)]
pub enum ToggleScriptFailure {
    /// The script could not be run (spawn failure) or overran the runner's timeout
    /// and was killed.
    Command(CommandError),
    /// The script ran and exited unsuccessfully.
    Failed {
        /// The exit code, or `None` when the process was terminated by a signal.
        code: Option<i32>,
        /// The start of the captured stderr, bounded and marked like the Apply
        /// pipeline's excerpts, or `None` when the script wrote nothing to stderr.
        stderr: Option<String>,
    },
}

impl fmt::Display for ToggleScriptFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToggleScriptFailure::Command(error) => {
                write!(f, "the toggle script could not be run: {error}")
            }
            ToggleScriptFailure::Failed { code, stderr } => {
                match code {
                    Some(code) => write!(f, "the toggle script exited with status {code}")?,
                    None => f.write_str("the toggle script was terminated by a signal")?,
                }
                match stderr {
                    Some(stderr) => write!(f, ": {stderr}"),
                    None => Ok(()),
                }
            }
        }
    }
}

impl std::error::Error for ToggleScriptFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ToggleScriptFailure::Command(error) => Some(error),
            ToggleScriptFailure::Failed { .. } => None,
        }
    }
}

/// The Display page's contribution to an [`ApplyPlan`](crate::core::apply::ApplyPlan):
/// the single `monitors.conf` write plus the value validations to re-check (R8.3).
#[derive(Debug)]
pub struct DisplayApply {
    /// The atomic write rewriting only the changed `monitor=` records.
    pub write: FileWrite,
    /// The staged monitor values to validate before writing (mode strings, positions,
    /// scale ranges), reusing the model validators (R8.3).
    pub validations: Vec<(SettingId, Value)>,
}

impl DisplayModel {
    /// Builds the model by probing the compositor and reading `monitors.conf` (the
    /// production entry point, called from the startup worker — architecture §8).
    ///
    /// Runs `hyprctl monitors all -j` through `runner` (`all` so a currently-disabled
    /// output still appears) and reads `monitors_conf_path`. Returns `None` when the
    /// probe cannot be run or reports failure — i.e. there is no live compositor to
    /// query — so the Display page degrades to a placeholder (R4.2). A readable
    /// probe with an unreadable `monitors.conf` still yields a model (the file-backed
    /// controls are then hidden, R4.4).
    ///
    /// `laptop_toggle_script` is the detected dotfiles toggle script (R8.5); `None`
    /// hides the laptop toggle (R4.2) — detection has already logged why.
    pub fn load(
        runner: &dyn CommandRunner,
        monitors_conf_path: PathBuf,
        laptop_toggle_script: Option<PathBuf>,
    ) -> Option<Self> {
        let command = monitors_probe();
        let output = match runner.run(&command) {
            Ok(output) if output.success() => output,
            Ok(_) => {
                tracing::info!(
                    "hyprctl reported failure listing monitors; Display page has no monitor data"
                );
                return None;
            }
            Err(error) => {
                tracing::info!(%error, "could not run hyprctl to list monitors; Display page hidden");
                return None;
            }
        };

        let json = String::from_utf8_lossy(output.stdout()).into_owned();
        // A read failure means the file is missing/unreadable (R4.4): keep the model
        // (built from the live probe) but with no records, so the file-backed controls
        // are hidden while the runtime laptop toggle still works.
        let monitors_conf = match fs::read_to_string(&monitors_conf_path) {
            Ok(contents) => Some(contents),
            Err(error) => {
                tracing::warn!(
                    path = %monitors_conf_path.display(),
                    %error,
                    "monitors.conf is missing or unreadable; per-monitor controls will be hidden (R4.4)"
                );
                None
            }
        };

        Some(Self::from_sources(
            monitors_conf.as_deref(),
            &json,
            monitors_conf_path,
            laptop_toggle_script,
        ))
    }

    /// Merges `monitors.conf` (if readable) with the `hyprctl monitors all -j` JSON into
    /// the page model — the pure core, tested with canned JSON (R6.2).
    ///
    /// Each live monitor from the JSON is matched to a specific `monitor=` record
    /// (exact name or `desc:` substring, last matching rule winning) and its
    /// configured mode/scale/position/enabled read from that record; an output with
    /// no specific record takes its baseline from the live state. `monitors_conf` is
    /// `None` when the file was unreadable (R4.4). Malformed JSON simply yields no
    /// monitors — this never panics.
    fn from_sources(
        monitors_conf: Option<&str>,
        hyprctl_json: &str,
        monitors_conf_path: PathBuf,
        laptop_toggle_script: Option<PathBuf>,
    ) -> Self {
        // The parser logs every grammar violation at `warn` with its line and rule;
        // the first one decides that the page is read-only (task 11.9).
        let records = monitors_conf.map(|text| MonitorsFile::parse(text).0);
        let records_problem = records
            .as_ref()
            .and_then(|file| file.grammar_violations().into_iter().next());
        if let Some(violation) = &records_problem {
            tracing::warn!(
                path = %monitors_conf_path.display(),
                %violation,
                "monitors.conf breaks its frozen grammar; per-monitor settings are read-only \
                 until the file is fixed (R8.3)"
            );
        }
        let live = parse_hyprctl_monitors(hyprctl_json);

        let monitors: Vec<Monitor> = live
            .into_iter()
            .map(|monitor| merge_monitor(monitor, records.as_ref()))
            .collect();

        // A laptop panel the script does not address gets no toggle (see
        // `TOGGLE_SCRIPT_OUTPUT`); say so, since detection found the script and the
        // user would otherwise wonder where the switch went (R4.2).
        if laptop_toggle_script.is_some() {
            for monitor in monitors
                .iter()
                .filter(|monitor| monitor.is_laptop && monitor.output_name != TOGGLE_SCRIPT_OUTPUT)
            {
                tracing::info!(
                    output = %monitor.output_name,
                    "the dotfiles laptop-display toggle script only switches {TOGGLE_SCRIPT_OUTPUT}; \
                     no laptop toggle is offered for this panel (R4.2)"
                );
            }
        }

        // Baseline monitors.conf from the exact bytes read, so a pre-write conflict
        // check catches an external edit since load (R5.6). Nothing is recorded when
        // the file was unreadable — there is then nothing to write and nothing to check.
        let mut freshness = FreshnessTracker::new();
        if let Some(text) = monitors_conf {
            freshness.record_bytes(monitors_conf_path.as_path(), text.as_bytes());
        }

        DisplayModel {
            monitors,
            records,
            records_problem,
            monitors_conf_path,
            laptop_toggle_script,
            freshness,
        }
    }

    /// The number of monitors the model holds.
    pub fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    /// Whether the per-monitor file-backed controls should be shown: `monitors.conf`
    /// was readable (R4.4) **and** follows its frozen grammar (task 11.9), so edits
    /// can be written. When `false`, every `stage_*` call is refused.
    pub fn records_editable(&self) -> bool {
        self.records.is_some() && self.records_problem.is_none()
    }

    /// The first frozen-grammar rule the loaded `monitors.conf` breaks — the reason
    /// the page is read-only, for the page to show — or `None` when the file follows
    /// the grammar or could not be read at all.
    pub fn records_problem(&self) -> Option<&GrammarViolation> {
        self.records_problem.as_ref()
    }

    /// A read-only view of monitor `index` — its identity, drop-down options and
    /// current values — or `None` when `index` names no monitor (task 9.24).
    ///
    /// Indices are positions in the order `hyprctl` reported the outputs, from `0`
    /// to [`Self::monitor_count`]` - 1`. The page builds each monitor's section from
    /// one view; an index held across a model rebuild (a Refresh or conflict reload
    /// can change the monitor count) yields `None` rather than a panic, so stale UI
    /// state can never take the app down (R8.3).
    pub fn monitor(&self, index: usize) -> Option<MonitorView<'_>> {
        let monitor = self.monitors.get(index)?;
        Some(MonitorView {
            monitor,
            laptop_toggle_script: self.laptop_toggle_script.as_deref(),
        })
    }

    /// Whether `monitors.conf` changed on disk since it was loaded (R5.6).
    ///
    /// The Apply glue calls this before writing a dirty monitor edit; a `true` result
    /// means another program edited the file, so the write must be aborted and the
    /// model re-loaded rather than clobbering the stale parse. Always `false` when the
    /// file was unreadable at load (nothing was baselined, and there is nothing to
    /// write).
    pub fn check_conflict(&self) -> bool {
        !self.freshness.check_conflicts().is_empty()
    }

    /// Re-reads `monitors.conf` and re-probes the compositor, returning a fresh model
    /// with a new freshness baseline (R5.6 "warn and re-load").
    ///
    /// Called after [`Self::check_conflict`] detects an external edit: the fresh model
    /// re-parses the current file (discarding the now-stale staged edits, which were
    /// based on the superseded bytes) so a subsequent Apply builds on the current
    /// contents. Returns `None` if the compositor can no longer be probed.
    pub fn reload(&self, runner: &dyn CommandRunner) -> Option<Self> {
        Self::load(
            runner,
            self.monitors_conf_path.clone(),
            self.laptop_toggle_script.clone(),
        )
    }

    /// Stages a new resolution for monitor `index`, composing it with a refresh rate
    /// valid for that resolution (keeping the current one when still available, else
    /// the first the compositor reports) into the mode field.
    ///
    /// Refused (nothing staged) when the composed mode is malformed or the resulting
    /// `monitors.conf` would break its frozen grammar (R8.3; see [`StageRefusal`]),
    /// and with [`StageRefusal::UnknownMonitor`] when `index` names no monitor.
    pub fn stage_resolution(
        &mut self,
        index: usize,
        resolution: String,
    ) -> Result<(), StageRefusal> {
        // A stale index has no refresh to keep; the composed mode is then never
        // staged, because `stage_checked` refuses the index before any value check.
        let refresh = self.monitor(index).and_then(|view| {
            let available = view.refresh_rate_options_for(&resolution);
            match view.effective_refresh() {
                Some(refresh) if available.contains(&refresh) => Some(refresh),
                _ => available.into_iter().next(),
            }
        });
        self.stage_mode(index, compose_mode(&resolution, refresh.as_deref()))
    }

    /// Stages a new refresh rate for monitor `index`, keeping its current resolution.
    /// Refused like [`Self::stage_resolution`].
    pub fn stage_refresh(&mut self, index: usize, refresh: String) -> Result<(), StageRefusal> {
        // As in `stage_resolution`, a stale index is refused by `stage_checked`
        // before the (then meaningless) composed mode is looked at.
        let resolution = self
            .monitor(index)
            .map(|view| view.effective_resolution())
            .unwrap_or_default();
        self.stage_mode(index, compose_mode(&resolution, Some(&refresh)))
    }

    /// Stages a new scale for monitor `index`, refusing a value that is not a number or
    /// lies outside the plausible range (R8.3), or whose record would break the frozen
    /// grammar, so the drop-down snaps back rather than writing a broken scale.
    pub fn stage_scale(&mut self, index: usize, scale: String) -> Result<(), StageRefusal> {
        let valid = check_value(&scale)
            .map_err(StageRefusal::Grammar)
            .and_then(|()| {
                let value = scale
                    .parse::<f64>()
                    .map_err(|_| StageRefusal::ScaleNotANumber(scale.clone()))?;
                validate_float_range(value, &SCALE_RANGE).map_err(StageRefusal::Value)
            });
        self.stage_checked(index, valid, |config| config.scale = scale)
    }

    /// Stages a new position for monitor `index`, refusing a malformed position (R8.3)
    /// so the drop-down snaps back rather than writing a record neither
    /// `hyprland.lua`'s loader nor `scripts/hypr-display-profile.sh`'s awk could parse.
    ///
    /// The check is here, in the model, rather than being left to the curated
    /// [`MonitorView::position_options`] drop-down that feeds it today: the model is
    /// the enforcement layer, so a future free-text position control — or a position
    /// picked up from a hand-edited `monitors.conf` — cannot bypass it. See
    /// [`validate_monitor_position`] for what a position may look like and why a comma
    /// or `#` in one would break the record.
    pub fn stage_position(&mut self, index: usize, position: String) -> Result<(), StageRefusal> {
        let valid = check_value(&position)
            .map_err(StageRefusal::Grammar)
            .and_then(|()| validate_monitor_position(&position).map_err(StageRefusal::Value));
        self.stage_checked(index, valid, |config| config.position = position)
    }

    /// Stages an enable/disable edit for a **non-laptop** monitor `index` (a staged
    /// `monitors.conf` change).
    ///
    /// Refused for a laptop panel, whose on/off state is the runtime toggle
    /// ([`Self::toggle_laptop`], R5.2). Disabling is refused with
    /// [`GrammarRule::DisableForm`] named: the frozen grammar has no record form that
    /// turns an output off (the legacy `<output>,disable` record would leave it on
    /// under `hyprland.lua`'s loader — see the module docs). Enabling an output that is
    /// off writes a complete record and stages normally.
    pub fn stage_enabled(&mut self, index: usize, enabled: bool) -> Result<(), StageRefusal> {
        let is_laptop = self.monitor(index).is_some_and(|view| view.is_laptop());
        let valid = if is_laptop {
            Err(StageRefusal::LaptopEnablement)
        } else {
            Ok(())
        };
        self.stage_checked(index, valid, |config| config.enabled = enabled)
    }

    /// Stages the mode field for monitor `index`, refusing a malformed mode (R8.3).
    ///
    /// Like every `stage_*` value, the raw mode is first checked for a character the
    /// frozen grammar cannot carry ([`check_value`]), so a value such as
    /// `1920x1080@60,bitdepth` — which would insert a `bitdepth` key without a value —
    /// is refused naming that grammar rule rather than a generic format error.
    fn stage_mode(&mut self, index: usize, mode: String) -> Result<(), StageRefusal> {
        let valid = check_value(&mode)
            .map_err(StageRefusal::Grammar)
            .and_then(|()| validate_monitor_mode(&mode).map_err(StageRefusal::Value));
        self.stage_checked(index, valid, |config| config.mode = mode)
    }

    /// The shared tail of every `stage_*` call: when `index` names a monitor and the
    /// caller's value checks (`valid`) passed, applies `edit` to a copy of that
    /// monitor's effective config and stages it only when the whole resulting
    /// `monitors.conf` follows the frozen grammar (task 11.9, R8.3).
    ///
    /// The index is checked first, so an index left over from before a model rebuild
    /// is reported as [`StageRefusal::UnknownMonitor`] rather than as whatever its
    /// value check would say about a monitor that no longer exists (task 9.24).
    ///
    /// The grammar check is a trial render of every pending edit — this one included
    /// — into a copy of the parsed file, followed by the same whole-file validation
    /// [`Self::apply_contribution`] runs before writing. Validating the rendered file
    /// rather than the changed value is what makes it a whole-record check: the
    /// record's untouched fields and extras, and the file-level rules (one record per
    /// laptop-panel key, the catch-all first), are covered too. An edit back to the
    /// original needs no check — it unstages. A refused edit — by the index, the value
    /// checks or the render — leaves the model unchanged and is logged at `warn`.
    fn stage_checked(
        &mut self,
        index: usize,
        valid: Result<(), StageRefusal>,
        edit: impl FnOnce(&mut MonitorConfig),
    ) -> Result<(), StageRefusal> {
        let result = if index < self.monitors.len() {
            valid.and_then(|()| self.try_stage(index, edit))
        } else {
            Err(StageRefusal::UnknownMonitor { index })
        };
        if let Err(refusal) = &result {
            let output = self
                .monitors
                .get(index)
                .map_or("(none)", |monitor| monitor.output_name.as_str());
            tracing::warn!(index, %output, %refusal, "refusing a monitor edit (R8.3)");
        }
        result
    }

    /// Stages `scale` for monitor `index` **without any check**, for tests of the
    /// writer's last line of defence (R8.3, task 11.9).
    ///
    /// Every real `stage_*` call trial-renders its edit, so a staged state the writer
    /// refuses cannot be reached through them; this seam lets an integration test
    /// craft one and prove [`Self::apply_contribution`] still refuses it before any
    /// write. Compiled only for tests and the `testing` feature. Unlike the real
    /// `stage_*` methods it panics on an out-of-range `index`, so a test that
    /// addresses the wrong monitor fails loudly instead of silently staging nothing.
    #[cfg(any(test, feature = "testing"))]
    pub fn stage_scale_unchecked_for_tests(&mut self, index: usize, scale: &str) {
        let monitor = &mut self.monitors[index];
        let mut staged = monitor.effective().clone();
        staged.scale = scale.to_string();
        monitor.staged = Some(staged);
    }

    /// [`Self::stage_checked`] without the logging or the up-front index check.
    ///
    /// The candidate edit is made on a copy of all monitors, which is trial-rendered
    /// and then becomes the model's state only when accepted. Reaching the monitor
    /// through `get_mut` on that copy, rather than indexing, keeps this free of a
    /// panic path even if a future caller skips `stage_checked`.
    fn try_stage(
        &mut self,
        index: usize,
        edit: impl FnOnce(&mut MonitorConfig),
    ) -> Result<(), StageRefusal> {
        if let Some(violation) = &self.records_problem {
            return Err(StageRefusal::FileBreaksGrammar(violation.clone()));
        }
        let Some(records) = &self.records else {
            return Err(StageRefusal::RecordsUnavailable);
        };

        let mut monitors = self.monitors.clone();
        let Some(monitor) = monitors.get_mut(index) else {
            return Err(StageRefusal::UnknownMonitor { index });
        };
        let mut candidate = monitor.effective().clone();
        edit(&mut candidate);
        // An edit back to the original unstages, so re-selecting the current value
        // never leaves the page dirty — and needs no grammar check.
        if candidate == monitor.original {
            monitor.staged = None;
        } else {
            monitor.staged = Some(candidate);
            render_checked(&mut records.clone(), &monitors)?;
        }

        self.monitors = monitors;
        Ok(())
    }

    /// Whether any monitor has a pending file-backed edit — the Display page's dirty
    /// state, which the window folds into the global Apply/Reset chrome (R5.1).
    pub fn is_dirty(&self) -> bool {
        self.monitors.iter().any(Monitor::is_dirty)
    }

    /// Discards every staged monitor edit, returning the page to clean (R5.1).
    pub fn reset(&mut self) {
        for monitor in &mut self.monitors {
            monitor.staged = None;
        }
    }

    /// The Display page's contribution to the Apply plan.
    ///
    /// It clones the parsed file, applies each dirty monitor's edits through the
    /// surgical [`monitors`](crate::parsers::monitors) parser (so only the targeted
    /// records change), re-validates the whole result against the frozen grammar (the
    /// last line of defence of R8.3, task 11.9), and emits the complete new bytes as a
    /// [`FileWrite`] for the shared pipeline, alongside the value validations (R8.3).
    ///
    /// Returns:
    /// - `Ok(None)` when there is nothing to write — no monitor is dirty (the common
    ///   clean case); `monitors.conf` was unreadable at load, so there is no parse to
    ///   edit and the page hides the file-backed rows anyway (R4.4); or the dirty edits
    ///   render no change to the file (see the inline comment on that branch);
    /// - `Ok(Some(contribution))` with the rendered write and its validations;
    /// - `Err(DisplayWriteError)` when there *are* dirty monitor edits but the write
    ///   cannot be produced. The caller must **abort the Apply** in this case rather
    ///   than skip the write, since [`Self::commit`] would otherwise promote the staged
    ///   monitor values and re-baseline the file's freshness against bytes that were
    ///   never written (see [`DisplayWriteError`]). Near-unreachable in practice: every
    ///   stage already passed the same render and grammar check (R8.3), so this needs a
    ///   staged state that bypassed the `stage_*` methods.
    pub fn apply_contribution(&self) -> Result<Option<DisplayApply>, DisplayWriteError> {
        if !self.is_dirty() {
            return Ok(None);
        }
        let Some(mut file) = self.records.clone() else {
            return Ok(None);
        };
        let changed_keys = render_checked(&mut file, &self.monitors).inspect_err(|error| {
            tracing::error!(%error, "refusing to write monitors.conf; aborting the apply (R8.3)");
        })?;
        // A dirty monitor does not always render an edit. A `,disable` record on disk
        // makes the page read-only, so the remaining case is an output that is off with
        // no specific record (only the catch-all configures it): the staged and original
        // `enabled` flags are both false and `apply_edits` writes nothing for it (see its
        // `else if staged.enabled` guard), so a staged mode/scale edit on such an output
        // leaves `changed_keys` empty. That is reachable through this model's API — the
        // Display page hides the mode/scale rows of a disabled output, so it cannot
        // happen from the UI today. `Ok(None)` is the right answer: nothing is written,
        // so the window commits nothing and the staged edit simply stays pending (no
        // desync), which is preferable to emitting a byte-identical no-op write.
        if changed_keys.is_empty() {
            return Ok(None);
        }

        Ok(Some(DisplayApply {
            write: FileWrite {
                path: self.monitors_conf_path.clone(),
                contents: file.emit().into_bytes(),
                changed_keys,
                backing: BackingFile::MonitorsConf,
                // The mode/scale/position values in this write are exactly what the
                // `validations` below hand the pipeline to re-check (R8.3).
                validation: WriteValidation::InPlan,
                mode: WriteMode::Replace,
            },
            validations: self.staged_validations(),
        }))
    }

    /// Commits the staged edits after a successful Apply: applies them to the in-memory
    /// `monitors.conf` and promotes each monitor's staged config to its original, so
    /// the model reflects what was just written and the page is clean again.
    pub fn commit(&mut self) {
        // Apply the staged edits to the in-memory records and capture the resulting
        // bytes — the exact bytes the pipeline just wrote.
        let written = if let Some(records) = self.records.as_mut() {
            if let Err(error) = apply_edits(records, &self.monitors) {
                // The write already succeeded (Apply returned Applied), so this only
                // keeps the in-memory copy in step; a mismatch is logged, not fatal.
                tracing::error!(%error, "failed to update the in-memory monitors.conf after apply");
            }
            Some(records.emit())
        } else {
            None
        };
        // Re-baseline monitors.conf from the just-written bytes, so the app's own write
        // is not mistaken for an external conflict on the next Apply (R5.6).
        if let Some(bytes) = &written {
            self.freshness
                .record_bytes(self.monitors_conf_path.as_path(), bytes.as_bytes());
        }

        for monitor in &mut self.monitors {
            if let Some(staged) = monitor.staged.take() {
                monitor.original = staged;
            }
        }

        // Refresh each monitor's record addressing so a first-configure's freshly
        // appended record is edited in place (surgical `set_field`) on the next Apply
        // rather than appended a second time (N8).
        if let Some(records) = &self.records {
            for monitor in &mut self.monitors {
                monitor.record_name =
                    match_record(records, &monitor.output_name, &monitor.description);
                let enabled = monitor
                    .record_name
                    .as_ref()
                    .and_then(|name| records.is_enabled(name));
                monitor.has_enabled_record = enabled == Some(true);
            }
        }
    }

    /// The value validations for the staged edits (R8.3): each enabled, edited monitor
    /// contributes its mode, position, and scale to be re-checked by the pipeline before
    /// any write, reusing the [`SettingId`] validators.
    ///
    /// The whole staged record is validated, not only the field the user touched, because
    /// every field of it is about to be written into the same `monitor=` line.
    fn staged_validations(&self) -> Vec<(SettingId, Value)> {
        let mut validations = Vec::new();
        for monitor in &self.monitors {
            if !monitor.is_dirty() {
                continue;
            }
            let config = monitor.effective();
            if !config.enabled {
                continue;
            }
            validations.push((SettingId::MonitorMode, Value::Enum(config.mode.clone())));
            validations.push((
                SettingId::MonitorPosition,
                Value::Enum(config.position.clone()),
            ));
            if let Ok(scale) = config.scale.parse::<f64>() {
                validations.push((SettingId::MonitorScale, Value::Float(scale)));
            }
        }
        validations
    }

    /// Turns the laptop panel `index` on (`enable == true`) or off right away by
    /// running the dotfiles' toggle script (runtime-only, R5.2; task 11.8; analysis
    /// §7.3, §8.1).
    ///
    /// `scripts/hypr-toggle-laptop-display` is a **pure toggle** — no arguments, it
    /// flips whatever state it finds — and it owns every step of a switch: the live
    /// `hl.monitor` call, the hotplug watcher's `/tmp/hypr-laptop-display-forced`
    /// override, the desktop notification and the eww bar relaunch. The app only
    /// brackets it with reads, issuing exactly this sequence through `runner`:
    ///
    /// 1. `hyprctl monitors all -j` — the panel's current state (`all` also lists a
    ///    disabled output, with `"disabled": true`). When the panel already is in the
    ///    requested state — the hotplug watcher or the keybind changed it since the
    ///    page was drawn — nothing else runs: the pure toggle would flip it *away*
    ///    from what the user asked for. The switch just catches up with the read.
    /// 2. the script, with no arguments. It inherits the app's environment (the
    ///    [`SystemCommandRunner`](crate::system::command::SystemCommandRunner) never
    ///    clears it), which it needs: `HYPRLAND_INSTANCE_SIGNATURE` to reach the
    ///    compositor and `HOME` to find its helper scripts.
    /// 3. `hyprctl monitors all -j` again — the state the switch shows afterwards.
    ///
    /// Only that second read decides the outcome, never the script's exit status:
    /// the script ends with the bar relaunch, whose failure says nothing about the
    /// panel, and the runner's 5 s timeout can stop it after the monitor call already
    /// went through. A changed state is success (a script failure alongside it is
    /// logged at `warn`); an unchanged state is [`LaptopToggleError::Unchanged`].
    /// The stored state — and with it [`MonitorView::laptop_enabled`] and the
    /// switch — is only ever updated from a read. Every failure is logged and
    /// returned for the page to show; none is fatal (R5.5). An `index` that names no
    /// monitor is [`LaptopToggleError::Unavailable`] and runs nothing.
    ///
    /// This never stages, never becomes dirty, and never touches `monitors.conf`.
    pub fn toggle_laptop(
        &mut self,
        index: usize,
        enable: bool,
        runner: &dyn CommandRunner,
    ) -> Result<(), LaptopToggleError> {
        // An index that names no monitor (stale UI state) is unavailable too.
        let Some((output, script)) = self.monitor(index).and_then(|view| {
            let script = view.toggle_program()?;
            Some((view.name().to_string(), script.to_string()))
        }) else {
            return Err(LaptopToggleError::Unavailable);
        };

        let before = read_output_enabled(runner, &output).map_err(|error| {
            tracing::error!(
                %output,
                %error,
                "could not read the laptop panel's state; the toggle script was not run (R5.5)"
            );
            LaptopToggleError::ReadBefore(error)
        })?;
        self.set_live_enabled(index, before);
        if before == enable {
            tracing::info!(
                %output,
                enabled = before,
                "the laptop panel is already in the requested state; the toggle script was \
                 not run (R5.2)"
            );
            return Ok(());
        }

        let script_failure = run_toggle_script(runner, &script).err();

        let after = match read_output_enabled(runner, &output) {
            Ok(after) => after,
            Err(error) => {
                tracing::error!(
                    %output,
                    %error,
                    "the laptop-display toggle script ran, but the panel's new state could \
                     not be read (R5.5)"
                );
                return Err(LaptopToggleError::ReadAfter {
                    error,
                    script: script_failure,
                });
            }
        };
        self.set_live_enabled(index, after);

        if after == before {
            tracing::error!(
                %output,
                enabled = after,
                "the laptop panel did not change state after the toggle script (R5.5)"
            );
            return Err(LaptopToggleError::Unchanged {
                enabled: after,
                script: script_failure,
            });
        }
        if let Some(failure) = &script_failure {
            tracing::warn!(
                %output,
                %failure,
                "the laptop-display toggle script reported a failure, but the panel did switch"
            );
        }
        tracing::info!(
            %output,
            enabled = after,
            "switched the laptop display with the dotfiles toggle script (R5.2)"
        );
        Ok(())
    }

    /// Stores the on/off state just read for monitor `index` — the only way
    /// [`MonitorView::laptop_enabled`] changes after load.
    ///
    /// [`Self::toggle_laptop`] calls this only for an index its availability check
    /// already resolved, so the monitor is always found; reaching it through
    /// `get_mut` rather than indexing keeps the toggle free of a panic path all the
    /// same.
    fn set_live_enabled(&mut self, index: usize, enabled: bool) {
        if let Some(monitor) = self.monitors.get_mut(index) {
            monitor.live_enabled = enabled;
        }
    }
}

/// A borrowed, read-only view of one monitor of a [`DisplayModel`], returned by
/// [`DisplayModel::monitor`] (task 9.24).
///
/// It answers everything the Display page renders for one output: the identity
/// reported by the compositor, the options of each drop-down, the value each control
/// currently shows, and whether the runtime laptop toggle is offered. Values come
/// from two places, and each getter says which:
///
/// - the **live** state, read from `hyprctl monitors all -j` at load (and, for the
///   laptop panel's on/off state, re-read around each toggle);
/// - the **effective** configuration — the pending staged edit if there is one,
///   otherwise the value configured by the output's `monitor=` record in
///   `monitors.conf`, or the live state when no specific enabled record configures
///   the output.
///
/// The view borrows the model, so it cannot outlive a mutation: after any `stage_*`
/// or toggle call, ask [`DisplayModel::monitor`] for a fresh one.
#[derive(Clone, Copy, Debug)]
pub struct MonitorView<'a> {
    /// The monitor this view reports on.
    monitor: &'a Monitor,
    /// The detected toggle script (the model's), needed to tell whether this
    /// monitor gets the runtime laptop toggle.
    laptop_toggle_script: Option<&'a Path>,
}

impl<'a> MonitorView<'a> {
    /// The live output name reported by the compositor (e.g. `eDP-1`, `DP-1`).
    pub fn name(&self) -> &'a str {
        &self.monitor.output_name
    }

    /// The compositor's description of the output (make and model, e.g.
    /// `AU Optronics 0x2036`), possibly empty.
    pub fn description(&self) -> &'a str {
        &self.monitor.description
    }

    /// Whether the output is an internal laptop panel (its live name starts with
    /// `eDP`, any case). A panel's on/off state is the runtime toggle (R5.2), never
    /// a staged edit; its mode and scale are staged like any monitor's.
    pub fn is_laptop(&self) -> bool {
        self.monitor.is_laptop
    }

    /// The resolution (`WIDTHxHEIGHT`) drop-down options: the distinct resolutions
    /// the compositor reports the output supports, plus the effective resolution
    /// (first) when it is not among them, so the current value is always selectable.
    pub fn resolution_options(&self) -> Vec<String> {
        let mut options: Vec<String> = Vec::new();
        for mode in &self.monitor.available {
            if !options.contains(&mode.resolution) {
                options.push(mode.resolution.clone());
            }
        }
        let configured = self.effective_resolution();
        if !options.contains(&configured) {
            options.insert(0, configured);
        }
        options
    }

    /// The refresh-rate drop-down options for the effective resolution: the refresh
    /// rates the compositor reports for that resolution, plus the effective refresh
    /// when set and not among them. Empty for a special mode token (`preferred`),
    /// whose refresh is Hyprland's choice.
    pub fn refresh_rate_options(&self) -> Vec<String> {
        self.refresh_rate_options_for(&self.effective_resolution())
    }

    /// The scale drop-down options: the effective scale (first, so it is
    /// preselected) followed by the curated common scales (`1` up to `2`) not
    /// already listed.
    pub fn scale_options(&self) -> Vec<String> {
        let configured = self.effective_scale();
        let mut options = vec![configured.clone()];
        for scale in CURATED_SCALES {
            if *scale != configured {
                options.push((*scale).to_string());
            }
        }
        options
    }

    /// The position drop-down options: the curated `auto` and `0x0`, plus the
    /// effective position when it is neither.
    pub fn position_options(&self) -> Vec<String> {
        let mut options: Vec<String> = CURATED_POSITIONS.iter().map(|p| (*p).to_string()).collect();
        let configured = self.effective_position();
        if !options.contains(&configured) {
            options.push(configured);
        }
        options
    }

    /// The effective resolution: the `WIDTHxHEIGHT` part of the effective mode
    /// field, or the whole field when it is a token such as `preferred`.
    pub fn effective_resolution(&self) -> String {
        split_mode(&self.monitor.effective().mode).0
    }

    /// The effective refresh rate (the part of the mode field after `@`), or `None`
    /// when the mode carries none (a bare `WIDTHxHEIGHT` or a token).
    pub fn effective_refresh(&self) -> Option<String> {
        split_mode(&self.monitor.effective().mode).1
    }

    /// The effective scale field (e.g. `1.066667`).
    pub fn effective_scale(&self) -> String {
        self.monitor.effective().scale.clone()
    }

    /// The effective position field (`XxY` or `auto`).
    pub fn effective_position(&self) -> String {
        self.monitor.effective().position.clone()
    }

    /// Whether the output is enabled in its effective configuration. This drives a
    /// non-laptop output's enable switch. For a laptop panel it is always `true` —
    /// the panel's enablement is not a record setting — and its live on/off state is
    /// [`Self::laptop_enabled`] instead.
    pub fn effective_enabled(&self) -> bool {
        self.monitor.effective().enabled
    }

    /// Whether the compositor last reported the output as on (`!disabled`). Only
    /// ever set from a read — at load, and by [`DisplayModel::toggle_laptop`] from
    /// the reads around the toggle script — and drives the runtime laptop switch's
    /// position, independent of any staged record edit.
    pub fn laptop_enabled(&self) -> bool {
        self.monitor.live_enabled
    }

    /// Whether this output gets the runtime laptop toggle (R4.2, R5.2; task 11.8):
    /// it is the output the dotfiles' toggle script switches (`eDP-1`, which the
    /// script hard-codes) and detection found the script. Otherwise the toggle is
    /// hidden, never shown greyed out; the panel's file-backed mode and scale
    /// controls are unaffected.
    pub fn laptop_toggle_available(&self) -> bool {
        self.toggle_program().is_some()
    }

    /// The toggle script to run for this output, as the program string of a
    /// [`Command`], or `None` when the laptop toggle is unavailable for it (see
    /// [`Self::laptop_toggle_available`]).
    ///
    /// Detection only reports a script whose path is valid UTF-8 (a [`Command`] holds
    /// its program as a `String`), so the `to_str` here cannot fail for a detected
    /// script; it is a plain conversion, never a lossy one.
    fn toggle_program(&self) -> Option<&'a str> {
        if self.monitor.output_name != TOGGLE_SCRIPT_OUTPUT {
            return None;
        }
        self.laptop_toggle_script?.to_str()
    }

    /// The refresh-rate options for a specific `resolution`: the refreshes the
    /// compositor reports for it, plus the effective refresh when `resolution` is
    /// the effective one and that refresh is not otherwise listed. Empty for a
    /// special mode token.
    fn refresh_rate_options_for(&self, resolution: &str) -> Vec<String> {
        if is_special_mode(resolution) {
            return Vec::new();
        }
        let mut options: Vec<String> = Vec::new();
        for mode in &self.monitor.available {
            if mode.resolution == resolution && !options.contains(&mode.refresh) {
                options.push(mode.refresh.clone());
            }
        }
        // Keep the configured refresh selectable when it applies to this resolution.
        if self.effective_resolution() == resolution {
            if let Some(refresh) = self.effective_refresh() {
                if !options.contains(&refresh) {
                    options.insert(0, refresh);
                }
            }
        }
        options
    }
}

/// The compositor probe both the page load and the laptop toggle read the monitors
/// from: `hyprctl monitors all -j`. `all` matters — without it a disabled output is
/// simply missing from the list, while with it the output stays listed with
/// `"disabled": true`, so the page can show a switched-off panel at all.
fn monitors_probe() -> Command {
    Command::new("hyprctl").args(["monitors", "all", "-j"])
}

/// Reads whether `output` is enabled from a fresh [`monitors_probe`] (task 11.8).
fn read_output_enabled(runner: &dyn CommandRunner, output: &str) -> Result<bool, PanelReadError> {
    let reply = runner
        .run(&monitors_probe())
        .map_err(PanelReadError::Command)?;
    if !reply.success() {
        return Err(PanelReadError::Failed { code: reply.code() });
    }
    // Malformed JSON parses to no monitors (logged by the parser), which reads as
    // the output missing — either way the state is unknown.
    parse_hyprctl_monitors(&String::from_utf8_lossy(reply.stdout()))
        .into_iter()
        .find(|monitor| monitor.name == output)
        .map(|monitor| !monitor.disabled)
        .ok_or_else(|| PanelReadError::OutputMissing {
            output: output.to_string(),
        })
}

/// Runs the dotfiles' toggle script `program` with no arguments (task 11.8).
///
/// The failure, if any, is returned for [`DisplayModel::toggle_laptop`] to weigh
/// against its re-read; the runner has already logged the invocation (R7.3).
fn run_toggle_script(runner: &dyn CommandRunner, program: &str) -> Result<(), ToggleScriptFailure> {
    let reply = runner
        .run(&Command::new(program))
        .map_err(ToggleScriptFailure::Command)?;
    if reply.success() {
        Ok(())
    } else {
        Err(ToggleScriptFailure::Failed {
            code: reply.code(),
            stderr: stderr_excerpt(reply.stderr()),
        })
    }
}

/// Applies each dirty monitor's staged edits to `file`, returning the changed-key
/// labels for logging (R7.3). Shared by [`DisplayModel::apply_contribution`] (on a
/// clone) and [`DisplayModel::commit`] (in place) so both render edits identically.
///
/// An in-place field edit ([`MonitorField`]) preserves trailing extras such as
/// `bitdepth,10`; a (re-)enable or a first-time configure writes the whole record body
/// with [`MonitorsFile::set_record`]. A staged disable is refused with
/// [`GrammarRule::DisableForm`]: the frozen grammar has no form that turns an output
/// off (see the module docs), so there is nothing valid to write. A laptop panel's
/// enablement is never staged (it is runtime-only) and never reaches this.
fn apply_edits(file: &mut MonitorsFile, monitors: &[Monitor]) -> Result<Vec<String>, EditError> {
    let mut changed = Vec::new();
    for monitor in monitors {
        let Some(staged) = &monitor.staged else {
            continue;
        };
        if staged == &monitor.original {
            continue;
        }
        let name = monitor.address();

        if staged.enabled != monitor.original.enabled {
            if !staged.enabled {
                return Err(EditError::Grammar(GrammarRule::DisableForm));
            }
            file.set_record(name, &active_record(staged))?;
            changed.push(format!("monitor {} enabled", monitor.output_name));
        } else if staged.enabled {
            if monitor.has_enabled_record {
                // In-place field edits preserve the leading `monitor=<name>` token,
                // the untouched fields, and any trailing extras (awk-parseable).
                if staged.mode != monitor.original.mode {
                    file.set_field(name, MonitorField::Mode, &staged.mode)?;
                    changed.push(format!("monitor {} mode", monitor.output_name));
                }
                if staged.position != monitor.original.position {
                    file.set_field(name, MonitorField::Position, &staged.position)?;
                    changed.push(format!("monitor {} position", monitor.output_name));
                }
                if staged.scale != monitor.original.scale {
                    file.set_field(name, MonitorField::Scale, &staged.scale)?;
                    changed.push(format!("monitor {} scale", monitor.output_name));
                }
            } else {
                // No enabled record yet (catch-all configured, or none): write a full
                // specific record so this output wins later-rule precedence.
                file.set_record(name, &active_record(staged))?;
                changed.push(format!("monitor {} configured", monitor.output_name));
            }
        }
    }
    Ok(changed)
}

/// The [`MonitorRecord`] of a config, sourcing mode/position/scale from the (staged)
/// values — used when (re-)enabling or first-configuring an output.
fn active_record(config: &MonitorConfig) -> MonitorRecord {
    MonitorRecord {
        mode: config.mode.clone(),
        position: config.position.clone(),
        scale: config.scale.clone(),
    }
}

/// Renders every staged edit of `monitors` into `file` ([`apply_edits`]) and then
/// validates the **whole** result against the frozen grammar, returning the changed
/// keys only when both succeed (task 11.9, R8.3).
///
/// The parser's edit methods already refuse a record they would break, but only by
/// the rules located on the edited line; this whole-file pass is what guarantees the
/// bytes handed to the writer are readable by both downstream parsers, whatever
/// combination of edits produced them. Shared by the stage-time trial render
/// ([`DisplayModel::stage_checked`]) and [`DisplayModel::apply_contribution`], so a
/// stage that was accepted always renders at Apply.
fn render_checked(
    file: &mut MonitorsFile,
    monitors: &[Monitor],
) -> Result<Vec<String>, DisplayWriteError> {
    let changed = apply_edits(file, monitors).map_err(DisplayWriteError::Render)?;
    match file.grammar_violations().into_iter().next() {
        Some(violation) => Err(DisplayWriteError::Grammar(violation)),
        None => Ok(changed),
    }
}

/// A monitor as reported by `hyprctl monitors all -j`, before merging with the records.
struct LiveMonitor {
    name: String,
    description: String,
    width: i64,
    height: i64,
    refresh: f64,
    scale: f64,
    position: (i64, i64),
    disabled: bool,
    available: Vec<Mode>,
}

/// Parses the `hyprctl monitors all -j` JSON array into [`LiveMonitor`]s, skipping any
/// entry without a name. Malformed JSON yields an empty list — never a panic.
fn parse_hyprctl_monitors(json: &str) -> Vec<LiveMonitor> {
    let parsed: JsonValue = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not parse hyprctl monitors JSON; no monitors listed");
            return Vec::new();
        }
    };
    let Some(entries) = parsed.as_array() else {
        return Vec::new();
    };

    entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_string();
            let available = entry
                .get("availableModes")
                .and_then(JsonValue::as_array)
                .map(|modes| {
                    modes
                        .iter()
                        .filter_map(|mode| mode.as_str())
                        .filter_map(parse_mode_string)
                        .collect()
                })
                .unwrap_or_default();
            Some(LiveMonitor {
                name,
                description: entry
                    .get("description")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                width: entry.get("width").and_then(JsonValue::as_i64).unwrap_or(0),
                height: entry.get("height").and_then(JsonValue::as_i64).unwrap_or(0),
                refresh: entry
                    .get("refreshRate")
                    .and_then(JsonValue::as_f64)
                    .unwrap_or(0.0),
                scale: entry
                    .get("scale")
                    .and_then(JsonValue::as_f64)
                    .unwrap_or(1.0),
                position: (
                    entry.get("x").and_then(JsonValue::as_i64).unwrap_or(0),
                    entry.get("y").and_then(JsonValue::as_i64).unwrap_or(0),
                ),
                disabled: entry
                    .get("disabled")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false),
                available,
            })
        })
        .collect()
}

/// Merges one live monitor with its matching `monitor=` record (if any) into the page
/// [`Monitor`].
fn merge_monitor(live: LiveMonitor, records: Option<&MonitorsFile>) -> Monitor {
    let is_laptop = live
        .name
        .to_ascii_lowercase()
        .starts_with(LAPTOP_OUTPUT_PREFIX);
    // The specific record configuring this output, together with the file it was
    // found in. Keeping the two in one `Option` makes "a record name implies a
    // records file" hold by construction: a name only ever comes out of searching a
    // file, so there is no state in which code below has a name but no file to read
    // it from, and no unreachable branch to guard.
    let matched = records.and_then(|file| {
        match_record(file, &live.name, &live.description).map(|name| (file, name))
    });

    // Read the configured values from a specific, enabled record; otherwise fall back
    // to the live state so the drop-downs still show something meaningful.
    let record_enabled = matched
        .as_ref()
        .and_then(|(file, name)| file.is_enabled(name));
    let has_enabled_record = record_enabled == Some(true);

    let live_mode = live_mode_string(&live);
    let live_scale = trim_float(live.scale);
    let live_position = format!("{}x{}", live.position.0, live.position.1);

    let enabled_record = if has_enabled_record {
        matched.as_ref().map(|(file, name)| (*file, name.as_str()))
    } else {
        None
    };
    let (mode, position, scale) = if let Some((file, name)) = enabled_record {
        (
            file.field(name, MonitorField::Mode)
                .map(str::to_string)
                .unwrap_or(live_mode),
            file.field(name, MonitorField::Position)
                .map(str::to_string)
                .unwrap_or(live_position),
            file.field(name, MonitorField::Scale)
                .map(str::to_string)
                .unwrap_or(live_scale),
        )
    } else {
        (live_mode, live_position, live_scale)
    };

    // A laptop panel's enablement is the runtime toggle, not a staged record edit, so
    // its *config* enabled flag always reads as enabled and is never staged — this
    // assumes the eDP `monitor=` record is kept in active form (the single-source
    // invariant, analysis §6.2; a `,disable` record breaks the frozen grammar and
    // makes the page read-only). A non-laptop output follows its record, falling back
    // to the live `disabled` flag when no record configures it.
    let enabled = if is_laptop {
        true
    } else {
        record_enabled.unwrap_or(!live.disabled)
    };

    let original = MonitorConfig {
        enabled,
        mode,
        position,
        scale,
    };

    Monitor {
        output_name: live.name,
        description: live.description,
        is_laptop,
        live_enabled: !live.disabled,
        record_name: matched.map(|(_, name)| name),
        has_enabled_record,
        available: live.available,
        original,
        staged: None,
    }
}

/// Finds the `monitor=` record that specifically configures the output named
/// `output_name` with description `description`: an exact name match or a `desc:`
/// substring match, with the **last** matching rule winning (Hyprland's later-rule-wins
/// precedence). The catch-all (`` empty name) is never treated as a specific record —
/// editing a catch-all-only output appends its own record instead.
fn match_record(file: &MonitorsFile, output_name: &str, description: &str) -> Option<String> {
    let mut matched: Option<String> = None;
    for name in file.record_names() {
        if name.is_empty() {
            continue;
        }
        let is_match = name == output_name
            || name
                .strip_prefix("desc:")
                .map(str::trim)
                .is_some_and(|want| !want.is_empty() && description.contains(want));
        if is_match {
            matched = Some(name.to_string());
        }
    }
    matched
}

/// The live mode string `WIDTHxHEIGHT@REFRESH` for a monitor, or `preferred` when it
/// reports no active resolution (e.g. a currently-disabled output).
fn live_mode_string(live: &LiveMonitor) -> String {
    if live.width > 0 && live.height > 0 {
        format!(
            "{}x{}@{}",
            live.width,
            live.height,
            trim_float(live.refresh)
        )
    } else {
        "preferred".to_string()
    }
}

/// Parses one `availableModes` entry (`2560x1440@60.01Hz`) into a [`Mode`], or `None`
/// when it is not a `WIDTHxHEIGHT@REFRESH` string.
fn parse_mode_string(raw: &str) -> Option<Mode> {
    let (resolution, rest) = raw.split_once('@')?;
    // Require the `WIDTHxHEIGHT` shape; a mode entry without an `x` is not a resolution.
    resolution.split_once('x')?;
    let hz: f64 = rest.trim().trim_end_matches("Hz").trim().parse().ok()?;
    Some(Mode {
        resolution: resolution.to_string(),
        refresh: trim_float(hz),
    })
}

/// Splits a mode field into its resolution and optional refresh: `2560x1440@60` →
/// (`2560x1440`, `Some("60")`); a bare `WIDTHxHEIGHT` or a token → (that, `None`).
fn split_mode(mode: &str) -> (String, Option<String>) {
    match mode.split_once('@') {
        Some((resolution, refresh)) => (resolution.to_string(), Some(refresh.to_string())),
        None => (mode.to_string(), None),
    }
}

/// Composes a mode field from a resolution and optional refresh. A special token
/// (`preferred`) or a resolution with no refresh yields the resolution alone;
/// otherwise `WIDTHxHEIGHT@REFRESH`.
fn compose_mode(resolution: &str, refresh: Option<&str>) -> String {
    match refresh {
        Some(refresh) if !is_special_mode(resolution) => format!("{resolution}@{refresh}"),
        _ => resolution.to_string(),
    }
}

/// Whether a resolution field is a Hyprland special mode token rather than a
/// `WIDTHxHEIGHT` (in which case it carries no refresh rate).
fn is_special_mode(resolution: &str) -> bool {
    matches!(resolution, "preferred" | "highres" | "highrr")
}

/// Formats a float without a trailing `.0`, trimming the noise `hyprctl` reports:
/// `120.0` → `120`, `60.012` → `60.012`, `1.07` → `1.07`. Rust's default `f64`
/// `Display` already gives the shortest round-tripping form, so this only strips a
/// bare trailing `.0`.
fn trim_float(value: f64) -> String {
    let text = format!("{value}");
    text.strip_suffix(".0").map(str::to_string).unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::command::{Command, CommandError, CommandOutput, MockCommandRunner};

    /// A realistic `hyprctl monitors all -j` payload for the personal laptop: one eDP-1
    /// output (desc `AU Optronics 0x2036`) with a single available mode, scale 1.07.
    const HYPRCTL_LAPTOP: &str = r#"[
        {
            "name": "eDP-1",
            "description": "AU Optronics 0x2036",
            "width": 2560,
            "height": 1440,
            "refreshRate": 60.01200,
            "x": 0,
            "y": 0,
            "scale": 1.07,
            "disabled": false,
            "availableModes": ["2560x1440@60.01Hz"]
        }
    ]"#;

    /// A `monitors.conf` mirroring the real dotfiles: catch-all, a generic eDP-1 rule,
    /// and a later `desc:` rule (which wins for the personal laptop) plus an external.
    const MONITORS_CONF: &str = "\
# Dynamic monitor configuration
monitor=,preferred,auto,1
monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth,10
monitor=desc:AU Optronics 0x2036,2560x1440,auto,1.066667
monitor=desc:Lenovo Group Limited P24q-10 U4P00001,2560x1440,0x0,1
";

    /// The rendered Apply contribution of a dirty model, failing the test if the write
    /// could not be prepared. The abort path itself (a rendering failure, task 9.5) has
    /// its own test below.
    fn rendered_contribution(model: &DisplayModel) -> DisplayApply {
        model
            .apply_contribution()
            .expect("the monitors.conf write renders")
            .expect("a dirty edit produces a write")
    }

    /// The bytes a dirty model's contribution would write, as text.
    fn rendered_text(model: &DisplayModel) -> String {
        String::from_utf8(rendered_contribution(model).write.contents).expect("utf-8")
    }

    /// Monitor `index`'s view, failing the test when the model has no such monitor.
    fn view(model: &DisplayModel, index: usize) -> MonitorView<'_> {
        model.monitor(index).expect("the monitor exists")
    }

    /// Builds a laptop-only model without a detected toggle script, from the canned
    /// sources — the merge under test.
    fn laptop_model(dir: &std::path::Path) -> DisplayModel {
        DisplayModel::from_sources(
            Some(MONITORS_CONF),
            HYPRCTL_LAPTOP,
            dir.join("monitors.conf"),
            None,
        )
    }

    #[test]
    fn merge_pairs_the_live_output_with_its_later_winning_desc_record() {
        // The core merge (test #1): the live eDP-1 is matched to the LAST matching
        // record — the `desc:AU Optronics 0x2036` rule, which wins over the generic
        // `eDP-1` rule — so its configured mode/scale/position come from that record,
        // while its available modes come from the live probe.
        let dir = tempfile::tempdir().expect("temp dir");
        let model = laptop_model(dir.path());

        assert_eq!(model.monitor_count(), 1);
        assert_eq!(view(&model, 0).name(), "eDP-1");
        assert_eq!(view(&model, 0).description(), "AU Optronics 0x2036");
        assert!(
            view(&model, 0).is_laptop(),
            "eDP-1 is the internal laptop panel"
        );
        assert!(model.records_editable());

        // Configured values from the winning desc record (not the generic eDP-1 rule).
        assert_eq!(view(&model, 0).effective_resolution(), "2560x1440");
        assert_eq!(
            view(&model, 0).effective_refresh(),
            None,
            "the record's mode is a bare WIDTHxHEIGHT, so no refresh is preselected"
        );
        assert_eq!(view(&model, 0).effective_scale(), "1.066667");
        assert_eq!(view(&model, 0).effective_position(), "auto");

        // Available modes come from the live probe.
        assert_eq!(
            view(&model, 0).resolution_options(),
            vec!["2560x1440".to_string()]
        );
        assert_eq!(
            view(&model, 0).refresh_rate_options(),
            vec!["60.01".to_string()]
        );
        assert!(!model.is_dirty());
    }

    #[test]
    fn merge_runs_over_a_mock_command_runner_with_canned_json() {
        // Test #1, through the production `load` path: the merge is driven by
        // `hyprctl monitors all -j` via the injected MockCommandRunner, with the exact
        // arg vector asserted (no shell).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("monitors.conf");
        std::fs::write(&path, MONITORS_CONF).expect("write monitors.conf");

        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0,
            HYPRCTL_LAPTOP,
            "",
        ))]);
        let model =
            DisplayModel::load(&runner, path, None).expect("a successful probe yields a model");

        assert_eq!(model.monitor_count(), 1);
        assert_eq!(view(&model, 0).effective_scale(), "1.066667");
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").args(["monitors", "all", "-j"])],
            "the probe runs `hyprctl monitors all -j`, no shell"
        );
    }

    #[test]
    fn a_failed_probe_yields_no_model() {
        // No live compositor: a non-zero hyprctl exit degrades to no model, so the
        // window shows a placeholder rather than an empty page (R4.2).
        let dir = tempfile::tempdir().expect("temp dir");
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        assert!(DisplayModel::load(&runner, dir.path().join("monitors.conf"), None).is_none());
    }

    #[test]
    fn an_unreadable_monitors_conf_hides_file_backed_controls() {
        // R4.4: with a readable probe but no monitors.conf, the model still lists the
        // monitors (and the laptop toggle works) but the file-backed controls are
        // hidden and no write is produced.
        let dir = tempfile::tempdir().expect("temp dir");
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0,
            HYPRCTL_LAPTOP,
            "",
        ))]);
        let mut model = DisplayModel::load(&runner, dir.path().join("does-not-exist.conf"), None)
            .expect("the probe succeeded");

        assert_eq!(model.monitor_count(), 1);
        assert!(
            !model.records_editable(),
            "an unreadable monitors.conf hides the file-backed rows (R4.4)"
        );
        // An edit is refused — there is no record to edit — and produces no write.
        assert!(matches!(
            model.stage_scale(0, "1.5".to_string()),
            Err(StageRefusal::RecordsUnavailable)
        ));
        assert!(!model.is_dirty());
        assert!(matches!(model.apply_contribution(), Ok(None)));
    }

    #[test]
    fn staging_a_scale_produces_a_surgical_awk_parseable_write() {
        // Test #2 (the write): staging the laptop's scale rewrites only that record's
        // scale field, leaving the leading name token, the mode, and the position
        // untouched — so the record stays parseable by hypr-display-profile.sh's awk
        // (name field 1, mode field 2, scale field 4).
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());

        model
            .stage_scale(0, "1.25".to_string())
            .expect("the edit stages");
        assert!(model.is_dirty());

        let contribution = rendered_contribution(&model);
        assert_eq!(contribution.write.backing, BackingFile::MonitorsConf);
        let emitted = String::from_utf8(contribution.write.contents).expect("utf-8");

        // Only the desc:AU Optronics record's scale changed; every other line is
        // byte-identical.
        let expected = MONITORS_CONF.replace(
            "monitor=desc:AU Optronics 0x2036,2560x1440,auto,1.066667",
            "monitor=desc:AU Optronics 0x2036,2560x1440,auto,1.25",
        );
        assert_eq!(emitted, expected);

        // The edited record is still awk-parseable: split the whole line on commas and
        // confirm the positional fields.
        let line = emitted
            .lines()
            .find(|l| l.starts_with("monitor=desc:AU Optronics 0x2036,"))
            .expect("the edited record");
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(fields[0], "monitor=desc:AU Optronics 0x2036");
        assert_eq!(fields[1], "2560x1440", "field 2 is the mode");
        assert_eq!(fields[3], "1.25", "field 4 is the scale");

        // The validations re-check the whole staged record — mode, position, and scale
        // (R8.3) — since all three fields are written into the same line.
        assert!(
            contribution
                .validations
                .contains(&(SettingId::MonitorScale, Value::Float(1.25)))
        );
        assert!(
            contribution
                .validations
                .contains(&(SettingId::MonitorMode, Value::Enum("2560x1440".to_string())))
        );
        assert!(
            contribution
                .validations
                .contains(&(SettingId::MonitorPosition, Value::Enum("auto".to_string())))
        );
    }

    #[test]
    fn staging_a_mode_preserves_trailing_extras() {
        // Editing the mode of a record that carries extras (the generic eDP-1 rule has
        // `bitdepth,10`) must keep the extras. This exercises the eDP-1 record directly
        // by using a hyprctl payload whose only record match is the generic eDP-1 rule.
        let dir = tempfile::tempdir().expect("temp dir");
        // A monitors.conf where eDP-1 is the ONLY matching record (no later desc rule).
        let conf = "monitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth,10\n";
        let hyprctl = r#"[{"name":"eDP-1","description":"Internal","width":2880,"height":1800,"refreshRate":120.0,"x":0,"y":0,"scale":1.333333,"disabled":false,"availableModes":["2880x1800@120.00Hz","1920x1200@60.00Hz"]}]"#;
        let mut model =
            DisplayModel::from_sources(Some(conf), hyprctl, dir.path().join("monitors.conf"), None);

        model
            .stage_resolution(0, "1920x1200".to_string())
            .expect("the edit stages");
        let emitted = rendered_text(&model);
        assert_eq!(
            emitted, "monitor=eDP-1,1920x1200@60,auto,1.333333,bitdepth,10\n",
            "the mode changed to the new resolution@refresh; scale and the bitdepth extras are preserved"
        );
    }

    #[test]
    fn re_selecting_the_current_value_clears_dirty() {
        // Staging a value equal to the original clears the staged edit, so the page is
        // not dirty (mirrors the store).
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        model
            .stage_scale(0, "1.25".to_string())
            .expect("the edit stages");
        assert!(model.is_dirty());
        model
            .stage_scale(0, "1.066667".to_string())
            .expect("the edit stages");
        assert!(
            !model.is_dirty(),
            "editing back to the original clears dirty"
        );
        assert!(matches!(model.apply_contribution(), Ok(None)));
    }

    #[test]
    fn reset_discards_staged_edits() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        model
            .stage_scale(0, "1.5".to_string())
            .expect("the edit stages");
        assert!(model.is_dirty());
        model.reset();
        assert!(!model.is_dirty());
        assert_eq!(view(&model, 0).effective_scale(), "1.066667");
    }

    #[test]
    fn commit_promotes_staged_and_updates_the_records() {
        // After a successful apply, commit makes the staged value the new baseline and
        // keeps the in-memory records in step, so a further edit builds on it.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        model
            .stage_scale(0, "1.5".to_string())
            .expect("the edit stages");
        model.commit();
        assert!(!model.is_dirty(), "commit clears dirty");
        assert_eq!(
            view(&model, 0).effective_scale(),
            "1.5",
            "the staged value is the new original"
        );

        // A subsequent write renders from the committed records: only the scale differs
        // from the committed state.
        model
            .stage_position(0, "0x0".to_string())
            .expect("the edit stages");
        let emitted = rendered_text(&model);
        assert!(emitted.contains("monitor=desc:AU Optronics 0x2036,2560x1440,0x0,1.5"));
    }

    #[test]
    fn an_out_of_range_scale_is_rejected() {
        // R8.3: an implausible scale is refused on stage, so it never reaches a write.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        assert!(matches!(
            model.stage_scale(0, "9".to_string()),
            Err(StageRefusal::Value(ValidationError::OutOfRange { .. }))
        ));
        assert!(!model.is_dirty(), "an out-of-range scale must not stage");
        assert!(matches!(
            model.stage_scale(0, "large".to_string()),
            Err(StageRefusal::ScaleNotANumber(_))
        ));
        assert!(!model.is_dirty(), "a non-numeric scale must not stage");
    }

    #[test]
    fn a_malformed_position_is_rejected_but_a_valid_one_stages() {
        // Task 9.5 / R8.3: `stage_position` validates like every other staging path, so
        // a value that would break the `monitor=` record never becomes staged state. The
        // model is the enforcement layer — these values cannot come from the curated
        // drop-down today, but nothing may rely on that.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());

        for malformed in [
            // A comma would add a positional field and shift the fields both downstream
            // parsers read (analysis §7.2): refused by the frozen grammar, named as such.
            "1920,0",
            // A `#` would start a comment that both parsers strip: the grammar again.
            "0x0 # here",
            // The rest are well-formed for the grammar but not positions.
            "left",
            "1920",
            "1920.5x0",
        ] {
            let refusal = model
                .stage_position(0, malformed.to_string())
                .expect_err("a malformed position is refused");
            if malformed.contains([',', '#']) {
                assert!(
                    matches!(
                        refusal,
                        StageRefusal::Grammar(GrammarRule::ForbiddenCharacter { .. })
                    ),
                    "`{malformed}` is refused by the grammar: {refusal:?}"
                );
            } else {
                assert!(
                    matches!(
                        refusal,
                        StageRefusal::Value(ValidationError::InvalidMonitorPosition { .. })
                    ),
                    "`{malformed}` is refused by the position validator: {refusal:?}"
                );
            }
            assert!(
                !model.is_dirty(),
                "`{malformed}` must not stage as a position"
            );
            assert_eq!(
                view(&model, 0).effective_position(),
                "auto",
                "the rejected value must leave the shown position untouched"
            );
        }

        // A well-formed coordinate pair still stages, and reaches the record.
        model
            .stage_position(0, "-1920x0".to_string())
            .expect("the edit stages");
        assert!(model.is_dirty());
        assert!(
            rendered_text(&model)
                .contains("monitor=desc:AU Optronics 0x2036,2560x1440,-1920x0,1.066667")
        );
    }

    #[test]
    fn an_empty_position_field_on_disk_makes_the_plan_fail_validation() {
        // Task 9.5, documenting a deliberate consequence of validating the whole staged
        // record: a record whose position field is *empty* (`monitor=DP-1,preferred,,1`,
        // which Hyprland cannot place either) contributes an invalid position, so the
        // pipeline's validate-everything-first step (R8.3) aborts the Apply even though
        // the user only changed the scale.
        //
        // Rejecting is the intended behaviour, for two reasons. The mode field is
        // validated the same unconditional way, so this is one rule rather than a special
        // case for positions; and the (re-)enable/first-configure path writes a whole
        // record body from the staged values, which would otherwise put the empty
        // position into a record the app itself wrote. The user is told which value is
        // wrong and fixes the record.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf = "monitor=DP-1,preferred,,1\n";
        let hyprctl = r#"[{"name":"DP-1","description":"Ext","width":1920,"height":1080,"refreshRate":60.0,"x":0,"y":0,"scale":1.0,"disabled":false,"availableModes":["1920x1080@60.00Hz"]}]"#;
        let mut model =
            DisplayModel::from_sources(Some(conf), hyprctl, dir.path().join("monitors.conf"), None);
        assert_eq!(
            view(&model, 0).effective_position(),
            "",
            "the empty position field is read as an empty value"
        );

        model
            .stage_scale(0, "1.25".to_string())
            .expect("the edit stages");
        let contribution = rendered_contribution(&model);
        // The scale edit itself renders (the record has a scale field to rewrite)…
        assert!(
            String::from_utf8(contribution.write.contents.clone())
                .expect("utf-8")
                .contains("monitor=DP-1,preferred,,1.25")
        );
        // …but the plan carries the invalid position, which the pipeline rejects before
        // touching the file.
        let position = contribution
            .validations
            .iter()
            .find(|(id, _)| *id == SettingId::MonitorPosition)
            .expect("the staged record's position is validated");
        assert!(
            position.0.validate(&position.1).is_err(),
            "an empty position field must fail validation, aborting the Apply"
        );
    }

    /// A one-output `hyprctl monitors all -j` reply for an external `DP-1`.
    const HYPRCTL_EXTERNAL: &str = r#"[{"name":"DP-1","description":"Ext","width":1920,"height":1080,"refreshRate":60.0,"x":0,"y":0,"scale":1.0,"disabled":false,"availableModes":["1920x1080@60.00Hz"]}]"#;

    #[test]
    fn a_render_failure_aborts_the_apply_and_keeps_the_staged_edit() {
        // Task 9.5: when the writer cannot render a dirty edit, the contribution is an
        // Err — not an `Ok(None)` the window would read as "nothing to write" and follow
        // with a commit, promoting values that reached no file (R8.3).
        //
        // Every `stage_*` call already trial-renders its edit (task 11.9), so the failure
        // is injected by crafting the staged state directly, bypassing them: a staged
        // disable, which the writer refuses because the frozen grammar has no record
        // form that turns an output off.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf = "monitor=,preferred,auto,1\nmonitor=DP-1,1920x1080@60,0x0,1\n";
        let mut model = DisplayModel::from_sources(
            Some(conf),
            HYPRCTL_EXTERNAL,
            dir.path().join("monitors.conf"),
            None,
        );
        let mut crafted = model.monitors[0].original.clone();
        crafted.enabled = false;
        model.monitors[0].staged = Some(crafted);
        assert!(model.is_dirty());

        let error = model
            .apply_contribution()
            .expect_err("an unrenderable edit must abort the apply, not look like a no-op");
        assert!(
            matches!(
                error,
                DisplayWriteError::Render(EditError::Grammar(GrammarRule::DisableForm))
            ),
            "the failure is a writer rejection naming the rule: {error}"
        );

        // The staged edit survives the aborted Apply, so the user can change it and try
        // again.
        assert!(model.is_dirty(), "the staged edit must be retained");
        assert!(!view(&model, 0).effective_enabled());
        // Nothing was promoted: dropping the staged edit restores the on-disk baseline,
        // proving the model was never re-baselined against unwritten bytes.
        model.reset();
        assert!(view(&model, 0).effective_enabled());
    }

    #[test]
    fn the_writer_refuses_a_crafted_record_as_the_last_line_of_defence() {
        // R8.3 "validate all before any write": whatever reached the staged state, the
        // bytes handed to the writer are checked once more. Both injections bypass the
        // `stage_*` methods.
        let dir = tempfile::tempdir().expect("temp dir");

        // (a) A value with a comma — it would insert a `bitdepth` key without a value
        // into the record — is refused by the parser's own value check.
        let conf = "monitor=,preferred,auto,1\nmonitor=DP-1,1920x1080@60,0x0,1\n";
        let mut model = DisplayModel::from_sources(
            Some(conf),
            HYPRCTL_EXTERNAL,
            dir.path().join("monitors.conf"),
            None,
        );
        let mut crafted = model.monitors[0].original.clone();
        crafted.scale = "1.25,bitdepth".to_string();
        model.monitors[0].staged = Some(crafted);
        assert!(matches!(
            model.apply_contribution(),
            Err(DisplayWriteError::Render(EditError::Grammar(
                GrammarRule::ForbiddenCharacter { character: ',' }
            )))
        ));

        // (b) A file-level rule broken on a line the edit never touches (a catch-all
        // after a specific rule) is caught by the whole-file pass, which the parser's
        // per-line check alone would miss. Such a file loads read-only, so the staged
        // edit can only be crafted.
        let conf = "monitor=DP-1,1920x1080@60,0x0,1\nmonitor=,preferred,auto,1\n";
        let mut model = DisplayModel::from_sources(
            Some(conf),
            HYPRCTL_EXTERNAL,
            dir.path().join("monitors.conf"),
            None,
        );
        let mut crafted = model.monitors[0].original.clone();
        crafted.scale = "1.25".to_string();
        model.monitors[0].staged = Some(crafted);
        let error = model
            .apply_contribution()
            .expect_err("the whole-file check refuses the write");
        match &error {
            DisplayWriteError::Grammar(violation) => {
                assert_eq!(violation.line(), 2);
                assert_eq!(
                    violation.rule(),
                    &GrammarRule::CatchAllNotFirst {
                        first_record_line: 1
                    }
                );
            }
            other => panic!("expected the whole-file grammar refusal, got {other:?}"),
        }
        assert!(error.to_string().contains("monitors.conf"), "{error}");
    }

    #[test]
    fn disabling_a_non_laptop_monitor_is_refused_with_the_rule_named() {
        // A non-laptop output's enable switch stages a monitors.conf edit, but the frozen
        // grammar has no record form that turns an output off: the legacy
        // `<output>,disable` record would reach Hyprland through hyprland.lua's loader as
        // `mode = "disable"`, which Hyprland rejects while leaving the output on. The
        // stage is refused, naming that rule, and nothing becomes dirty.
        let dir = tempfile::tempdir().expect("temp dir");
        let hyprctl = r#"[{"name":"DP-1","description":"Lenovo Group Limited P24q-10 U4P00001","width":2560,"height":1440,"refreshRate":60.0,"x":0,"y":0,"scale":1.0,"disabled":false,"availableModes":["2560x1440@60.00Hz"]}]"#;
        let mut model = DisplayModel::from_sources(
            Some(MONITORS_CONF),
            hyprctl,
            dir.path().join("monitors.conf"),
            None,
        );
        assert!(!view(&model, 0).is_laptop());
        assert!(view(&model, 0).effective_enabled());

        let refusal = model
            .stage_enabled(0, false)
            .expect_err("a disable has no valid record form");
        assert!(
            matches!(refusal, StageRefusal::Grammar(GrammarRule::DisableForm)),
            "{refusal:?}"
        );
        assert!(
            refusal.to_string().contains("`<output>,disable`"),
            "the refusal names the rule: {refusal}"
        );
        assert!(!model.is_dirty());
        assert!(view(&model, 0).effective_enabled());
    }

    #[test]
    fn a_value_that_would_insert_a_key_without_a_value_is_refused_naming_the_rule() {
        // Acceptance (task 11.9): an edit that would insert `bitdepth` without a value
        // is refused at stage time with the grammar rule named — the comma is caught
        // before the scale/mode validators, so the user reads which rule was broken.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());

        let refusal = model
            .stage_scale(0, "1.25,bitdepth".to_string())
            .expect_err("a scale with a comma is refused");
        assert!(
            matches!(
                refusal,
                StageRefusal::Grammar(GrammarRule::ForbiddenCharacter { character: ',' })
            ),
            "{refusal:?}"
        );
        assert!(
            refusal.to_string().contains("`,`"),
            "the message names the rule: {refusal}"
        );
        assert!(
            model
                .stage_refresh(0, "60,bitdepth".to_string())
                .is_err_and(|refusal| matches!(
                    refusal,
                    StageRefusal::Grammar(GrammarRule::ForbiddenCharacter { character: ',' })
                ))
        );
        assert!(!model.is_dirty(), "nothing was staged");
    }

    #[test]
    fn a_refused_stage_is_logged_at_warn_and_keeps_an_earlier_staged_edit() {
        // A refusal leaves the model exactly as it was: an edit staged before it
        // survives, and the refusal is in the journal.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        model
            .stage_scale(0, "1.25".to_string())
            .expect("the edit stages");

        let events = crate::testing::capture_logs(|| {
            let _ = model.stage_position(0, "0,0".to_string());
        });
        let warning = events
            .iter()
            .find(|event| event.message == "refusing a monitor edit (R8.3)")
            .expect("the refusal is logged");
        assert_eq!(warning.level, tracing::Level::WARN);
        assert!(warning.fields.contains("eDP-1"), "{}", warning.fields);

        assert_eq!(view(&model, 0).effective_scale(), "1.25");
        assert_eq!(view(&model, 0).effective_position(), "auto");
    }

    #[test]
    fn staging_on_a_laptop_panel_enablement_is_refused() {
        // The laptop panel's on/off state is the runtime toggle (R5.2), never a record.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        assert!(matches!(
            model.stage_enabled(0, false),
            Err(StageRefusal::LaptopEnablement)
        ));
        assert!(!model.is_dirty());
    }

    #[test]
    fn an_output_token_with_a_comma_is_refused_at_stage_time() {
        // A first configure appends a record keyed on the output's name, written
        // verbatim, so the token is checked like every value. Here it is a `desc:`
        // string carrying a comma, which would split into an extra field and shift
        // mode, position and scale. hyprctl never reports such a name, but the model
        // may not rely on that: the stage is refused, naming the rule.
        let dir = tempfile::tempdir().expect("temp dir");
        let hyprctl = r#"[{"name":"desc:Example Corp, Inc 0x0001","description":"Ext","width":1920,"height":1080,"refreshRate":60.0,"x":0,"y":0,"scale":1.0,"disabled":false,"availableModes":["1920x1080@60.00Hz"]}]"#;
        let mut model = DisplayModel::from_sources(
            Some("monitor=,preferred,auto,1\n"),
            hyprctl,
            dir.path().join("monitors.conf"),
            None,
        );
        let refusal = model
            .stage_scale(0, "1.25".to_string())
            .expect_err("the appended record would break the grammar");
        assert!(
            matches!(
                refusal,
                StageRefusal::Grammar(GrammarRule::ForbiddenCharacter { character: ',' })
            ),
            "{refusal:?}"
        );
        assert!(!model.is_dirty());
    }

    #[test]
    fn a_hand_edited_record_breaking_the_grammar_makes_the_page_read_only() {
        // The hand-edited invalid record decision (task 11.9): an eDP record with a
        // `bitdepth` key but no value breaks the frozen grammar, so the file loads
        // read-only — logged at `warn` with the line and the rule — and every edit is
        // refused naming them, rather than editing around a record the app would never
        // write. The runtime laptop toggle is unaffected.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf =
            "monitor=,preferred,auto,1\nmonitor=eDP-1,2880x1800@120,auto,1.333333,bitdepth\n";
        let hyprctl = HYPRCTL_LAPTOP.replace("AU Optronics 0x2036", "Internal Panel");
        let mut model = None;
        let events = crate::testing::capture_logs(|| {
            model = Some(DisplayModel::from_sources(
                Some(conf),
                &hyprctl,
                dir.path().join("monitors.conf"),
                Some(PathBuf::from(TOGGLE_SCRIPT)),
            ));
        });
        let mut model = model.expect("the model was built");

        let read_only = events
            .iter()
            .find(|event| event.message.contains("per-monitor settings are read-only"))
            .expect("the read-only decision is logged");
        assert_eq!(read_only.level, tracing::Level::WARN);
        assert!(read_only.fields.contains("line 2"), "{}", read_only.fields);
        assert!(
            read_only.fields.contains("key,value"),
            "{}",
            read_only.fields
        );

        assert!(!model.records_editable());
        let problem = model.records_problem().expect("the violation is reported");
        assert_eq!(problem.line(), 2);
        assert_eq!(problem.rule(), &GrammarRule::IncompleteExtra);
        assert!(
            view(&model, 0).laptop_toggle_available(),
            "the runtime toggle stays"
        );

        let refusal = model
            .stage_scale(0, "1.25".to_string())
            .expect_err("a read-only page refuses every edit");
        match &refusal {
            StageRefusal::FileBreaksGrammar(violation) => {
                assert_eq!(violation.rule(), &GrammarRule::IncompleteExtra);
            }
            other => panic!("expected the read-only refusal, got {other:?}"),
        }
        assert!(refusal.to_string().contains("line 2"), "{refusal}");
        assert!(!model.is_dirty());
        assert!(matches!(model.apply_contribution(), Ok(None)));
    }

    #[test]
    fn a_legacy_disable_record_makes_the_page_read_only() {
        // A hand-written `<output>,disable` record is outside the grammar too: the file
        // loads read-only rather than letting the app edit around it.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf = "monitor=,preferred,auto,1\nmonitor=DP-1,disable\n";
        let model = DisplayModel::from_sources(
            Some(conf),
            HYPRCTL_EXTERNAL,
            dir.path().join("monitors.conf"),
            None,
        );
        assert!(!model.records_editable());
        assert_eq!(
            model.records_problem().map(GrammarViolation::rule),
            Some(&GrammarRule::DisableForm)
        );
    }

    /// A placeholder path for the detected toggle script. The mock runner never
    /// executes anything, so the file need not exist.
    const TOGGLE_SCRIPT: &str = "/test/dotfiles/scripts/hypr-toggle-laptop-display";

    /// [`HYPRCTL_LAPTOP`] with the panel switched off, as `monitors all -j` lists it.
    fn hyprctl_laptop_disabled() -> String {
        let disabled = HYPRCTL_LAPTOP.replace(r#""disabled": false"#, r#""disabled": true"#);
        assert_ne!(disabled, HYPRCTL_LAPTOP, "the fixture must carry the field");
        disabled
    }

    /// A successful `hyprctl monitors all -j` reply carrying `json`.
    fn probe_reply(json: &str) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput::fake_with_streams(0, json, ""))
    }

    /// The exact command sequence of a toggle that ran the script: read, the script
    /// with no arguments, read again.
    fn toggle_sequence() -> Vec<Command> {
        vec![
            Command::new("hyprctl").args(["monitors", "all", "-j"]),
            Command::new(TOGGLE_SCRIPT),
            Command::new("hyprctl").args(["monitors", "all", "-j"]),
        ]
    }

    /// The laptop model with the toggle script detected, backed by a real
    /// `monitors.conf` at `conf_path` so a test can prove the file is never written.
    fn toggle_model(conf_path: &std::path::Path) -> DisplayModel {
        std::fs::write(conf_path, MONITORS_CONF).expect("write monitors.conf");
        DisplayModel::from_sources(
            Some(MONITORS_CONF),
            HYPRCTL_LAPTOP,
            conf_path.to_path_buf(),
            Some(PathBuf::from(TOGGLE_SCRIPT)),
        )
    }

    #[test]
    fn laptop_toggle_reads_runs_the_script_and_re_reads_without_touching_monitors_conf() {
        // Task 11.8 acceptance: the toggle issues exactly `hyprctl monitors all -j`,
        // then the dotfiles script with no arguments, then the re-read; the switch
        // follows the re-read; monitors.conf is never written (nor is any state file
        // — the model has no path to one any more).
        let dir = tempfile::tempdir().expect("temp dir");
        let conf_path = dir.path().join("monitors.conf");
        let mut model = toggle_model(&conf_path);
        assert!(view(&model, 0).laptop_toggle_available());
        assert!(
            view(&model, 0).laptop_enabled(),
            "the load probe reports the panel on"
        );

        // Off: the panel reads on, the script runs, the panel reads off.
        let runner = MockCommandRunner::with_outcomes([
            probe_reply(HYPRCTL_LAPTOP),
            Ok(CommandOutput::fake(0)),
            probe_reply(&hyprctl_laptop_disabled()),
        ]);
        model
            .toggle_laptop(0, false, &runner)
            .expect("a toggle the re-read confirms succeeds");
        assert!(
            !view(&model, 0).laptop_enabled(),
            "the switch shows the read state"
        );
        let recorded = runner.recorded();
        assert_eq!(recorded, toggle_sequence());
        assert!(
            recorded[1].args_slice().is_empty(),
            "the pure-toggle script takes no arguments"
        );

        // On again: the same sequence, the other way round.
        let runner = MockCommandRunner::with_outcomes([
            probe_reply(&hyprctl_laptop_disabled()),
            Ok(CommandOutput::fake(0)),
            probe_reply(HYPRCTL_LAPTOP),
        ]);
        model
            .toggle_laptop(0, true, &runner)
            .expect("a toggle the re-read confirms succeeds");
        assert!(view(&model, 0).laptop_enabled());
        assert_eq!(runner.recorded(), toggle_sequence());

        assert!(!model.is_dirty(), "the runtime toggle never stages");
        assert_eq!(
            std::fs::read_to_string(&conf_path).expect("read monitors.conf"),
            MONITORS_CONF,
            "the laptop toggle must never touch monitors.conf"
        );
    }

    #[test]
    fn an_unchanged_panel_after_the_script_is_an_error_and_the_switch_follows_the_read() {
        // The script is a pure toggle the app cannot see into: when the re-read shows
        // no change (the script failed without saying so, or the hotplug watcher
        // switched the panel straight back), the toggle fails and the switch keeps
        // the read state rather than the requested one.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = toggle_model(&dir.path().join("monitors.conf"));
        let runner = MockCommandRunner::with_outcomes([
            probe_reply(HYPRCTL_LAPTOP),
            Ok(CommandOutput::fake(0)),
            probe_reply(HYPRCTL_LAPTOP),
        ]);

        let error = model
            .toggle_laptop(0, false, &runner)
            .expect_err("no change is a failure");
        assert!(
            matches!(
                error,
                LaptopToggleError::Unchanged {
                    enabled: true,
                    script: None
                }
            ),
            "got {error:?}"
        );
        assert_eq!(
            error.to_string(),
            "the panel is still on after running the toggle script"
        );
        assert!(
            view(&model, 0).laptop_enabled(),
            "the switch stays on, as read"
        );
        assert_eq!(runner.recorded(), toggle_sequence());
    }

    #[test]
    fn the_script_exit_status_alone_never_decides_the_outcome() {
        // The script ends with the eww bar relaunch, which exits 1 when it finds no
        // usable monitor — after the panel already switched. A failed script with a
        // changed panel is therefore a success; with an unchanged panel the script's
        // failure explains the error.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = toggle_model(&dir.path().join("monitors.conf"));
        let runner = MockCommandRunner::with_outcomes([
            probe_reply(HYPRCTL_LAPTOP),
            Ok(CommandOutput::fake(1)),
            probe_reply(&hyprctl_laptop_disabled()),
        ]);
        model
            .toggle_laptop(0, false, &runner)
            .expect("the panel switched, so the toggle succeeded");
        assert!(!view(&model, 0).laptop_enabled());

        let runner = MockCommandRunner::with_outcomes([
            probe_reply(&hyprctl_laptop_disabled()),
            Ok(CommandOutput::fake_with_streams(
                2,
                "",
                "jq: command not found\n",
            )),
            probe_reply(&hyprctl_laptop_disabled()),
        ]);
        let error = model
            .toggle_laptop(0, true, &runner)
            .expect_err("the panel did not switch");
        match &error {
            LaptopToggleError::Unchanged {
                enabled: false,
                script:
                    Some(ToggleScriptFailure::Failed {
                        code: Some(2),
                        stderr,
                    }),
            } => assert_eq!(stderr.as_deref(), Some("jq: command not found")),
            other => panic!("expected an unchanged panel with the script failure, got {other:?}"),
        }
        assert_eq!(
            error.to_string(),
            "the panel is still off after running the toggle script; the toggle script \
             exited with status 2: jq: command not found"
        );
        assert!(!view(&model, 0).laptop_enabled());
    }

    #[test]
    fn a_timed_out_script_with_a_changed_panel_is_a_success_logged_at_warn() {
        // The other half of the exit-status rule: the runner's timeout can stop the
        // script during its bar relaunch, after the monitor call already went
        // through. The re-read shows the switch, so the toggle succeeds and the
        // script's failure is only a `warn`.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf_path = dir.path().join("monitors.conf");
        let mut outcome = None;
        let mut enabled = None;
        // `capture_logs` runs the action twice, so each run builds its own model and
        // runner.
        let events = crate::testing::capture_logs(|| {
            let mut model = toggle_model(&conf_path);
            let runner = MockCommandRunner::with_outcomes([
                probe_reply(HYPRCTL_LAPTOP),
                Err(CommandError::Timeout {
                    limit: std::time::Duration::from_secs(5),
                }),
                probe_reply(&hyprctl_laptop_disabled()),
            ]);
            outcome = Some(model.toggle_laptop(0, false, &runner));
            enabled = Some(view(&model, 0).laptop_enabled());
            assert_eq!(runner.recorded(), toggle_sequence());
        });

        assert!(
            matches!(outcome, Some(Ok(()))),
            "the panel switched, so the toggle succeeded: {outcome:?}"
        );
        assert_eq!(enabled, Some(false), "the switch shows the read state");
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::WARN
                    && event
                        .message
                        .contains("reported a failure, but the panel did switch")),
            "the script's timeout must be logged at warn, got {events:?}"
        );
    }

    #[test]
    fn a_panel_already_in_the_requested_state_runs_no_script() {
        // The page drew the panel on, but the hotplug watcher switched it off since.
        // Running the pure toggle now would turn it back on against the user's "off";
        // the pre-read catches that, nothing runs, and the switch catches up.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = toggle_model(&dir.path().join("monitors.conf"));
        assert!(view(&model, 0).laptop_enabled());
        let runner = MockCommandRunner::with_outcomes([probe_reply(&hyprctl_laptop_disabled())]);

        model
            .toggle_laptop(0, false, &runner)
            .expect("the requested state already holds");
        assert!(!view(&model, 0).laptop_enabled());
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").args(["monitors", "all", "-j"])],
            "only the read runs"
        );
    }

    #[test]
    fn a_failed_read_before_the_script_runs_nothing_else() {
        // Without the pre-read the pure toggle could flip the panel the wrong way, so
        // a failed read stops the toggle before the script.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = toggle_model(&dir.path().join("monitors.conf"));
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);

        let error = model
            .toggle_laptop(0, false, &runner)
            .expect_err("an unreadable state fails the toggle");
        assert!(
            matches!(
                error,
                LaptopToggleError::ReadBefore(PanelReadError::Failed { code: Some(1) })
            ),
            "got {error:?}"
        );
        assert!(error.to_string().contains("toggle script was not run"));
        assert_eq!(runner.recorded().len(), 1, "the script must not run");
        assert!(view(&model, 0).laptop_enabled(), "the state is unchanged");

        // A reply that does not list the panel is just as unreadable.
        let runner = MockCommandRunner::with_outcomes([probe_reply("[]")]);
        assert!(matches!(
            model.toggle_laptop(0, false, &runner),
            Err(LaptopToggleError::ReadBefore(
                PanelReadError::OutputMissing { .. }
            ))
        ));
        assert_eq!(runner.recorded().len(), 1);
    }

    #[test]
    fn a_failed_read_after_the_script_keeps_the_last_read_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = toggle_model(&dir.path().join("monitors.conf"));
        let runner = MockCommandRunner::with_outcomes([
            probe_reply(HYPRCTL_LAPTOP),
            Ok(CommandOutput::fake(0)),
            Err(CommandError::Timeout {
                limit: std::time::Duration::from_secs(5),
            }),
        ]);

        let error = model
            .toggle_laptop(0, false, &runner)
            .expect_err("an unknown outcome is a failure");
        assert!(
            matches!(
                error,
                LaptopToggleError::ReadAfter {
                    error: PanelReadError::Command(_),
                    script: None
                }
            ),
            "got {error:?}"
        );
        assert!(
            view(&model, 0).laptop_enabled(),
            "the switch keeps the state read before the script"
        );
        assert_eq!(runner.recorded(), toggle_sequence());
    }

    #[test]
    fn without_the_script_the_laptop_toggle_is_hidden_and_runs_nothing() {
        // R4.2: no detected script, no switch — and a caller that tries anyway gets
        // `Unavailable` without a single command being issued. The panel's
        // file-backed controls stay.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        assert!(view(&model, 0).is_laptop());
        assert!(!view(&model, 0).laptop_toggle_available());
        assert!(
            model.records_editable(),
            "the mode/scale rows are unaffected"
        );

        let runner = MockCommandRunner::new();
        assert!(matches!(
            model.toggle_laptop(0, false, &runner),
            Err(LaptopToggleError::Unavailable)
        ));
        assert!(runner.recorded().is_empty());
    }

    #[test]
    fn only_the_output_the_script_switches_gets_the_toggle() {
        // The script hard-codes eDP-1; a panel named eDP-2 would have the wrong
        // output flipped under its switch, so it gets none, and that is logged.
        let dir = tempfile::tempdir().expect("temp dir");
        let hyprctl = HYPRCTL_LAPTOP.replace("eDP-1", "eDP-2");
        let mut model = None;
        let events = crate::testing::capture_logs(|| {
            model = Some(DisplayModel::from_sources(
                Some(MONITORS_CONF),
                &hyprctl,
                dir.path().join("monitors.conf"),
                Some(PathBuf::from(TOGGLE_SCRIPT)),
            ));
        });
        let model = model.expect("built");
        assert!(view(&model, 0).is_laptop());
        assert!(!view(&model, 0).laptop_toggle_available());
        assert!(
            model.records_editable(),
            "the mode/scale rows are unaffected"
        );
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::INFO
                    && event.message.contains("only switches eDP-1")),
            "the missing toggle must be logged at info, got {events:?}"
        );
    }

    #[test]
    fn an_external_edit_to_monitors_conf_is_detected_before_apply() {
        // F2 (R5.6): a monitors.conf edited by another program between load and Apply
        // is detected by the freshness check, so the write can be aborted rather than
        // clobbering the stale parse.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf_path = dir.path().join("monitors.conf");
        std::fs::write(&conf_path, MONITORS_CONF).expect("write monitors.conf");
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0,
            HYPRCTL_LAPTOP,
            "",
        ))]);
        let mut model = DisplayModel::load(&runner, conf_path.clone(), None)
            .expect("a successful probe yields a model");

        assert!(
            !model.check_conflict(),
            "an untouched file is not a conflict"
        );
        model
            .stage_scale(0, "1.25".to_string())
            .expect("the edit stages");

        // Another program rewrites monitors.conf after the load-time baseline.
        std::fs::write(&conf_path, "monitor=,preferred,auto,1\n").expect("external edit");
        assert!(
            model.check_conflict(),
            "the external edit must be detected before the write (R5.6)"
        );
    }

    #[test]
    fn a_second_apply_after_commit_does_not_self_conflict() {
        // F2 (R5.6): committing re-baselines monitors.conf from the just-written bytes,
        // so the app's own write is not mistaken for an external edit on the next Apply.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf_path = dir.path().join("monitors.conf");
        std::fs::write(&conf_path, MONITORS_CONF).expect("write monitors.conf");
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0,
            HYPRCTL_LAPTOP,
            "",
        ))]);
        let mut model =
            DisplayModel::load(&runner, conf_path.clone(), None).expect("the probe succeeded");

        model
            .stage_scale(0, "1.25".to_string())
            .expect("the edit stages");
        let contribution = rendered_contribution(&model);
        // Mimic the pipeline writing the produced bytes to disk, then commit.
        std::fs::write(&conf_path, &contribution.write.contents).expect("apply writes the file");
        model.commit();

        assert!(!model.is_dirty(), "commit clears dirty");
        assert!(
            !model.check_conflict(),
            "the written bytes are the new baseline, so a second apply does not self-conflict"
        );
    }

    #[test]
    fn changing_resolution_picks_a_valid_refresh_for_the_new_resolution() {
        // When the resolution changes and the current refresh is not offered for it,
        // the first available refresh for the new resolution is composed into the mode.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf = "monitor=DP-1,3840x2160@60,0x0,1\n";
        let hyprctl = r#"[{"name":"DP-1","description":"Ext","width":3840,"height":2160,"refreshRate":60.0,"x":0,"y":0,"scale":1.0,"disabled":false,"availableModes":["3840x2160@60.00Hz","2560x1440@144.00Hz","2560x1440@120.00Hz"]}]"#;
        let mut model =
            DisplayModel::from_sources(Some(conf), hyprctl, dir.path().join("monitors.conf"), None);
        assert_eq!(view(&model, 0).effective_resolution(), "3840x2160");
        assert_eq!(view(&model, 0).effective_refresh(), Some("60".to_string()));

        // 60 is not offered for 2560x1440 (which offers 144 and 120), so the first
        // available (144) is chosen.
        model
            .stage_resolution(0, "2560x1440".to_string())
            .expect("the edit stages");
        assert_eq!(view(&model, 0).effective_resolution(), "2560x1440");
        assert_eq!(view(&model, 0).effective_refresh(), Some("144".to_string()));
        assert_eq!(
            view(&model, 0).refresh_rate_options(),
            vec!["144".to_string(), "120".to_string()]
        );
    }

    #[test]
    fn malformed_hyprctl_json_yields_no_monitors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let model = DisplayModel::from_sources(
            Some(MONITORS_CONF),
            "{ not json ]",
            dir.path().join("monitors.conf"),
            None,
        );
        assert_eq!(model.monitor_count(), 0);
        assert!(!model.is_dirty());
    }

    #[test]
    fn scale_and_position_option_lists_include_the_configured_value() {
        let dir = tempfile::tempdir().expect("temp dir");
        let model = laptop_model(dir.path());
        // The configured scale is first (preselected), followed by the curated set.
        let scales = view(&model, 0).scale_options();
        assert_eq!(scales[0], "1.066667");
        assert!(scales.contains(&"1.333333".to_string()));
        // The configured position ("auto") is present; the curated set adds "0x0".
        let positions = view(&model, 0).position_options();
        assert!(positions.contains(&"auto".to_string()));
        assert!(positions.contains(&"0x0".to_string()));
    }

    #[test]
    fn an_output_with_no_record_falls_back_to_live_and_appends_on_apply() {
        // F3(a): a live output that no specific `monitor=` record configures takes its
        // baseline from the live state, and editing it APPENDS a new specific record
        // (first-configure) rather than editing an unrelated line.
        let dir = tempfile::tempdir().expect("temp dir");
        // monitors.conf configures only eDP + a catch-all; DP-5 has no specific record.
        let conf = "monitor=,preferred,auto,1\nmonitor=eDP-1,2880x1800@120,auto,1.333333\n";
        let hyprctl = r#"[{"name":"DP-5","description":"Ext","width":1920,"height":1080,"refreshRate":60.0,"x":0,"y":0,"scale":1.0,"disabled":false,"availableModes":["1920x1080@60.00Hz"]}]"#;
        let mut model =
            DisplayModel::from_sources(Some(conf), hyprctl, dir.path().join("monitors.conf"), None);

        // The baseline comes from the live state (no record to read).
        assert_eq!(view(&model, 0).effective_resolution(), "1920x1080");
        assert_eq!(view(&model, 0).effective_refresh(), Some("60".to_string()));
        assert_eq!(view(&model, 0).effective_scale(), "1");
        assert!(view(&model, 0).effective_enabled());

        // Editing appends a specific DP-5 record after the last monitor= line, leaving
        // the catch-all and eDP lines untouched.
        model
            .stage_scale(0, "1.25".to_string())
            .expect("the edit stages");
        let emitted = rendered_text(&model);
        assert_eq!(
            emitted,
            "monitor=,preferred,auto,1\nmonitor=eDP-1,2880x1800@120,auto,1.333333\nmonitor=DP-5,1920x1080@60,0x0,1.25\n",
            "a first-configure appends a specific record sourced from the live state"
        );
    }

    #[test]
    fn re_enabling_a_disabled_monitor_sources_mode_and_scale_from_live() {
        // F3(b): an output that is off with no specific record (configured by the
        // catch-all only) takes its baseline from the live `hyprctl` state, and enabling
        // it appends a complete record with those live-sourced mode/position/scale.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf = "monitor=,preferred,auto,1\n";
        // The disabled output still reports its last-known mode via `monitors all -j`.
        let hyprctl = r#"[{"name":"DP-2","description":"Ext Vendor Model","width":3440,"height":1440,"refreshRate":100.0,"x":0,"y":0,"scale":1.0,"disabled":true,"availableModes":["3440x1440@100.00Hz"]}]"#;
        let mut model =
            DisplayModel::from_sources(Some(conf), hyprctl, dir.path().join("monitors.conf"), None);
        assert!(!view(&model, 0).is_laptop());
        assert!(
            !view(&model, 0).effective_enabled(),
            "with no record, the live disabled flag reads as off"
        );
        assert_eq!(view(&model, 0).effective_resolution(), "3440x1440");
        assert_eq!(view(&model, 0).effective_scale(), "1");

        // Enabling writes a full record sourced from those live values.
        model.stage_enabled(0, true).expect("the edit stages");
        let emitted = rendered_text(&model);
        assert_eq!(
            emitted, "monitor=,preferred,auto,1\nmonitor=DP-2,3440x1440@100,0x0,1\n",
            "enabling appends the live-sourced record via set_record"
        );
    }

    #[test]
    fn a_first_configure_then_second_edit_edits_in_place_after_commit() {
        // N8: after a first-configure appends a record and the apply is committed, a
        // second edit must edit that record in place (surgical set_field), not append a
        // duplicate — commit refreshes the monitor's record addressing.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf = "monitor=,preferred,auto,1\n";
        let hyprctl = r#"[{"name":"DP-7","description":"Ext","width":1920,"height":1080,"refreshRate":60.0,"x":0,"y":0,"scale":1.0,"disabled":false,"availableModes":["1920x1080@60.00Hz"]}]"#;
        let mut model =
            DisplayModel::from_sources(Some(conf), hyprctl, dir.path().join("monitors.conf"), None);

        // First edit: appends a specific DP-7 record, then commit.
        model
            .stage_scale(0, "1.25".to_string())
            .expect("the edit stages");
        model.commit();

        // Second edit: must edit the appended record in place, so there is exactly one
        // DP-7 record (no duplicate append).
        model
            .stage_scale(0, "1.5".to_string())
            .expect("the edit stages");
        let emitted = rendered_text(&model);
        assert_eq!(
            emitted.matches("monitor=DP-7,").count(),
            1,
            "the second edit edits the appended record in place, not appends again:\n{emitted}"
        );
        assert!(emitted.contains("monitor=DP-7,1920x1080@60,0x0,1.5"));
    }

    #[test]
    fn a_stale_monitor_index_yields_no_view_and_every_stage_refuses_it() {
        // Task 9.24: an index held across a model rebuild that dropped a monitor
        // must never panic. The view accessor answers `None`; every `stage_*`
        // refuses with `UnknownMonitor` — even for a value its own check would
        // reject, since the index is checked first — logs the refusal at `warn`,
        // and leaves the model clean.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        let stale = model.monitor_count();
        assert_eq!(stale, 1, "the laptop fixture lists one monitor");
        assert!(model.monitor(stale).is_none());
        assert!(model.monitor(usize::MAX).is_none());

        let mut results = Vec::new();
        let events = crate::testing::capture_logs(|| {
            // `capture_logs` runs the action twice; keep only the second run.
            results.clear();
            results.push(model.stage_resolution(stale, "2560x1440".to_string()));
            results.push(model.stage_refresh(stale, "60.01".to_string()));
            results.push(model.stage_scale(stale, "1.25".to_string()));
            results.push(model.stage_scale(stale, "not a number".to_string()));
            results.push(model.stage_position(stale, "0x0".to_string()));
            results.push(model.stage_enabled(stale, true));
        });
        for result in &results {
            assert!(
                matches!(result, Err(StageRefusal::UnknownMonitor { index }) if *index == stale),
                "{result:?}"
            );
        }
        let refusals = events
            .iter()
            .filter(|event| event.message == "refusing a monitor edit (R8.3)")
            .inspect(|event| assert_eq!(event.level, tracing::Level::WARN))
            .count();
        assert_eq!(refusals, results.len(), "every refusal is logged");
        assert!(!model.is_dirty(), "nothing was staged");
        assert_eq!(view(&model, 0).effective_scale(), "1.066667");
    }

    #[test]
    fn a_stale_monitor_index_runs_no_laptop_toggle() {
        // Task 9.24: with the script detected, a toggle on an index past the end is
        // `Unavailable` and issues no command — not a panic, and not a toggle of
        // whichever panel happens to be listed.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = toggle_model(&dir.path().join("monitors.conf"));
        assert!(view(&model, 0).laptop_toggle_available());

        let runner = MockCommandRunner::new();
        assert!(matches!(
            model.toggle_laptop(model.monitor_count(), false, &runner),
            Err(LaptopToggleError::Unavailable)
        ));
        assert!(runner.recorded().is_empty());
        assert!(
            view(&model, 0).laptop_enabled(),
            "the panel state is untouched"
        );
    }
}
