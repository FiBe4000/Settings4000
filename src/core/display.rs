//! GTK-free Display-page domain model (task 6.1; architecture §6 "Staging" + the
//! Display page; R2.3, R4.2, R4.4, R5.2, R5.4, R8.3, R6.2).
//!
//! # What this module is
//!
//! The Display page edits per-monitor resolution/refresh/scale/position (staged to
//! `config/hypr/monitors.conf`, applied on **Apply**) and offers a runtime-only
//! laptop-display toggle (applied immediately, R5.2). Unlike every other setting in
//! the app, monitors are **discovered at runtime** — their names, count, and the
//! modes they support come from `hyprctl monitors -j` — so they do not fit the
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
//! # The merge: `monitors.conf` records + `hyprctl monitors -j`
//!
//! [`DisplayModel::from_sources`] merges two views of each output (R2.3):
//!
//! - the **live** state from `hyprctl monitors -j` — which outputs exist, the modes
//!   they *support* (for the resolution/refresh drop-downs), and their current
//!   mode/scale/position/enabled state;
//! - the **configured** values from the matching `monitor=` record — the drop-downs'
//!   current selections. A record is matched to a live output by exact name
//!   (`eDP-1`) or a `desc:` substring, honouring Hyprland's later-rule-wins order.
//!
//! An output with no specific record is still shown (configured by the catch-all or
//! defaults); editing it appends a specific record. When `monitors.conf` cannot be
//! read at all (R4.4), the file-backed drop-downs are hidden ([`Self::records_editable`]
//! is `false`) but the runtime laptop toggle still works.
//!
//! # Keeping the edited record awk-parseable (CRITICAL)
//!
//! `scripts/hypr-display-profile.sh` is the single source for the eDP panel's mode
//! and scale: it `awk`-parses the matching `monitor=` record (name field 1, mode
//! field 2, scale field 4, extras after — analysis §6.2). Edits therefore go through
//! the [`monitors`](crate::parsers::monitors) parser, which rewrites only the target
//! field's byte span in place ([`set_field`](crate::parsers::monitors::MonitorsFile::set_field),
//! extras-preserving) or the whole record body when (re-)enabling a monitor
//! ([`set_state`](crate::parsers::monitors::MonitorsFile::set_state)); it rejects a
//! comma/newline/`#` in any written value, so the record stays awk-parseable. The
//! app never touches the script.
//!
//! # The laptop-display toggle is runtime-only (R5.2)
//!
//! The internal panel's on/off state is **not** a `monitor=…,disable` record — the
//! hotplug watcher (`scripts/hypr-monitor-hotplug`) would fight one. Instead the
//! toggle mirrors the dotfiles' own `scripts/hypr-toggle-laptop-display` exactly: it
//! applies the on/off state live with the granular `hyprctl keyword monitor` command
//! (`…,disable` to turn off; `…,<mode>,<position>,<scale>[,<extras>]` re-applied from
//! the record to turn on) and writes/removes `/tmp/hypr-laptop-display-forced` (the
//! watcher's manual override: present = keep the panel on rather than auto-disabling
//! it when docked). The keyword command — not `hyprctl reload`, which re-reads the
//! still-active record and so cannot turn the panel off — is what actually toggles the
//! panel. It never stages, never becomes dirty, and never touches `monitors.conf`
//! ([`Self::toggle_laptop`]). The panel's *mode and scale*, by contrast, live in the
//! eDP `monitor=` record and are edited like any other monitor (the single-source
//! gotcha) — only its enablement is runtime.

use std::fs;
use std::path::PathBuf;

use serde_json::Value as JsonValue;

use crate::core::apply::FileWrite;
use crate::core::freshness::FreshnessTracker;
use crate::core::model::{
    SCALE_RANGE, SettingId, Value, validate_float_range, validate_monitor_mode,
};
use crate::core::reload::BackingFile;
use crate::parsers::monitors::{MonitorField, MonitorState, MonitorsFile};
use crate::system::command::{Command, CommandRunner};

/// The hotplug state-file path the laptop-display toggle writes/removes (analysis
/// §4/§6.2). Its presence tells `scripts/hypr-monitor-hotplug` to keep the internal
/// panel on rather than auto-disabling it when an external monitor is connected. The
/// path is a field of [`DisplayModel`] (defaulting to this) so tests inject a
/// temporary path instead of touching the real `/tmp` file.
const LAPTOP_STATE_FILE: &str = "/tmp/hypr-laptop-display-forced";

/// Output-name prefix identifying an internal laptop panel (`eDP-1`, `eDP-2`, …),
/// matched case-insensitively. Such a monitor's enablement uses the runtime toggle
/// (R5.2) rather than a staged `monitors.conf` edit; its mode/scale are still edited
/// through its record (the single-source gotcha, analysis §6.2).
const LAPTOP_OUTPUT_PREFIX: &str = "edp";

/// Curated scale factors offered in the scale drop-down, in ascending order. The
/// monitor's currently-configured scale is always added too (see
/// [`DisplayModel::scale_options`]) so an unusual on-disk value stays selectable.
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
    /// Whether the compositor currently reports this output as enabled (`!disabled`
    /// from `hyprctl`). For a laptop panel this — not the record — drives the runtime
    /// enable switch's position (the panel's live on/off state), matching how
    /// `scripts/hypr-toggle-laptop-display` reads the current state; it is updated in
    /// place when [`DisplayModel::toggle_laptop`] flips the panel.
    live_enabled: bool,
    /// The `monitor=` record name that specifically configures this output (an exact
    /// name or a matched `desc:` rule), or `None` when only the catch-all/defaults
    /// apply. Edits address the record by this name; when `None`, the output name is
    /// used to append a new specific record.
    record_name: Option<String>,
    /// Whether a specific record for this output exists **and is enabled** — the
    /// condition under which an in-place [`set_field`](crate::parsers::monitors::MonitorsFile::set_field)
    /// edit applies; otherwise a full record is written with
    /// [`set_state`](crate::parsers::monitors::MonitorsFile::set_state).
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
/// and the runtime laptop-toggle state file (task 6.1).
///
/// See the module docs for the design. Built by [`Self::load`] (production) or
/// [`Self::from_sources`] (tests, with canned `hyprctl` JSON). Its file-backed edits
/// are surfaced to the Apply pipeline through [`Self::apply_contribution`]; its
/// runtime laptop toggle is [`Self::toggle_laptop`].
pub(crate) struct DisplayModel {
    /// The merged monitors, in the order `hyprctl` reported them.
    monitors: Vec<Monitor>,
    /// The parsed `monitors.conf`, or `None` when it could not be read (R4.4) — in
    /// which case the file-backed controls are hidden but the laptop toggle still
    /// works.
    records: Option<MonitorsFile>,
    /// The live XDG path of `monitors.conf`, the target of the produced [`FileWrite`]
    /// (R8.5). The writer canonicalizes it, so a symlink into a dotfiles repo has its
    /// real target rewritten and the link preserved.
    monitors_conf_path: PathBuf,
    /// The hotplug state-file path the laptop toggle writes/removes. Defaults to
    /// [`LAPTOP_STATE_FILE`]; overridden in tests.
    state_file: PathBuf,
    /// The freshness baseline for `monitors.conf`, recorded from the exact bytes read
    /// at load (R5.6). [`Self::apply_contribution`]'s caller conflict-checks against it
    /// before writing so an external edit is not clobbered, and [`Self::commit`]
    /// re-baselines it from the bytes just written so the app's own write is not
    /// mistaken for an external change on the next Apply. Empty when `monitors.conf`
    /// was unreadable (nothing to write, so nothing to check).
    freshness: FreshnessTracker,
}

/// The Display page's contribution to an [`ApplyPlan`](crate::core::apply::ApplyPlan):
/// the single `monitors.conf` write plus the value validations to re-check (R8.3).
pub(crate) struct DisplayApply {
    /// The atomic write rewriting only the changed `monitor=` records.
    pub(crate) write: FileWrite,
    /// The staged monitor values to validate before writing (mode strings, scale
    /// ranges), reusing the model validators (R8.3).
    pub(crate) validations: Vec<(SettingId, Value)>,
}

impl DisplayModel {
    /// Builds the model by probing the compositor and reading `monitors.conf` (the
    /// production entry point, called from the startup worker — architecture §8).
    ///
    /// Runs `hyprctl monitors all -j` through `runner` (`all` so a currently-disabled
    /// output still appears) and reads `monitors_conf_path`. Returns `None` when the
    /// probe cannot be run or reports failure — i.e. there is no live compositor to
    /// enumerate — so the Display page degrades to a placeholder (R4.2). A readable
    /// probe with an unreadable `monitors.conf` still yields a model (the file-backed
    /// controls are then hidden, R4.4).
    pub(crate) fn load(runner: &dyn CommandRunner, monitors_conf_path: PathBuf) -> Option<Self> {
        Self::load_with(runner, monitors_conf_path, PathBuf::from(LAPTOP_STATE_FILE))
    }

    /// [`Self::load`] with an explicit laptop state-file path, so tests exercise the
    /// runtime toggle against a temporary path instead of the real `/tmp` file.
    fn load_with(
        runner: &dyn CommandRunner,
        monitors_conf_path: PathBuf,
        state_file: PathBuf,
    ) -> Option<Self> {
        let command = Command::new("hyprctl").args(["monitors", "all", "-j"]);
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
            state_file,
        ))
    }

    /// Merges `monitors.conf` (if readable) with the `hyprctl monitors -j` JSON into
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
        state_file: PathBuf,
    ) -> Self {
        let records = monitors_conf.map(|text| MonitorsFile::parse(text).0);
        let live = parse_hyprctl_monitors(hyprctl_json);

        let monitors = live
            .into_iter()
            .map(|monitor| merge_monitor(monitor, records.as_ref()))
            .collect();

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
            monitors_conf_path,
            state_file,
            freshness,
        }
    }

    /// The number of monitors discovered.
    pub(crate) fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    /// Whether the per-monitor file-backed controls should be shown: `monitors.conf`
    /// was readable, so edits can be written (R4.4).
    pub(crate) fn records_editable(&self) -> bool {
        self.records.is_some()
    }

    /// The live output name of monitor `index` (e.g. `eDP-1`).
    pub(crate) fn monitor_name(&self, index: usize) -> &str {
        &self.monitors[index].output_name
    }

    /// The `hyprctl` description (make/model) of monitor `index`.
    pub(crate) fn monitor_description(&self, index: usize) -> &str {
        &self.monitors[index].description
    }

    /// Whether monitor `index` is an internal laptop panel — its enablement uses the
    /// runtime toggle (R5.2), its mode/scale the staged record edit.
    pub(crate) fn is_laptop(&self, index: usize) -> bool {
        self.monitors[index].is_laptop
    }

    /// The resolution (`WIDTHxHEIGHT`) drop-down options for monitor `index`: the
    /// distinct resolutions the compositor reports, plus the configured resolution
    /// when it is not among them, so the current value is always selectable.
    pub(crate) fn resolution_options(&self, index: usize) -> Vec<String> {
        let mut options: Vec<String> = Vec::new();
        for mode in &self.monitors[index].available {
            if !options.contains(&mode.resolution) {
                options.push(mode.resolution.clone());
            }
        }
        let configured = self.effective_resolution(index);
        if !options.contains(&configured) {
            options.insert(0, configured);
        }
        options
    }

    /// The refresh-rate drop-down options for monitor `index`, for its currently
    /// selected resolution: the refresh rates the compositor reports for that
    /// resolution, plus the configured refresh when set and not among them. Empty for
    /// a special mode token (`preferred`), whose refresh is Hyprland's choice.
    pub(crate) fn refresh_options(&self, index: usize) -> Vec<String> {
        let resolution = self.effective_resolution(index);
        self.refresh_options_for(index, &resolution)
    }

    /// The scale drop-down options for monitor `index`: the configured scale (first,
    /// so it is preselected) followed by the [`CURATED_SCALES`] not already listed.
    pub(crate) fn scale_options(&self, index: usize) -> Vec<String> {
        let configured = self.effective_scale(index);
        let mut options = vec![configured.clone()];
        for scale in CURATED_SCALES {
            if *scale != configured {
                options.push((*scale).to_string());
            }
        }
        options
    }

    /// The position drop-down options for monitor `index`: [`CURATED_POSITIONS`] plus
    /// the configured position when it is neither.
    pub(crate) fn position_options(&self, index: usize) -> Vec<String> {
        let mut options: Vec<String> = CURATED_POSITIONS.iter().map(|p| (*p).to_string()).collect();
        let configured = self.effective_position(index);
        if !options.contains(&configured) {
            options.push(configured);
        }
        options
    }

    /// The currently selected resolution (`WIDTHxHEIGHT` or a mode token) of monitor
    /// `index`, from its staged-or-original mode.
    pub(crate) fn effective_resolution(&self, index: usize) -> String {
        split_mode(&self.monitors[index].effective().mode).0
    }

    /// The currently selected refresh rate of monitor `index`, or `None` when the mode
    /// carries none (a bare `WIDTHxHEIGHT` or a token).
    pub(crate) fn effective_refresh(&self, index: usize) -> Option<String> {
        split_mode(&self.monitors[index].effective().mode).1
    }

    /// The currently selected scale of monitor `index`.
    pub(crate) fn effective_scale(&self, index: usize) -> String {
        self.monitors[index].effective().scale.clone()
    }

    /// The currently selected position of monitor `index`.
    pub(crate) fn effective_position(&self, index: usize) -> String {
        self.monitors[index].effective().position.clone()
    }

    /// Whether monitor `index` is currently enabled (staged-or-original). For a
    /// non-laptop output this drives its enable switch; a laptop panel's live
    /// enablement is the runtime toggle instead (see [`Self::laptop_forced_on`]).
    pub(crate) fn effective_enabled(&self, index: usize) -> bool {
        self.monitors[index].effective().enabled
    }

    /// Whether the laptop panel `index` is currently on, from the live `hyprctl`
    /// state (`!disabled`). This drives the runtime laptop enable switch's position —
    /// the panel's actual on/off state, as `scripts/hypr-toggle-laptop-display` reads
    /// it — independent of the staged record edits.
    pub(crate) fn laptop_enabled(&self, index: usize) -> bool {
        self.monitors[index].live_enabled
    }

    /// Whether `monitors.conf` changed on disk since it was loaded (R5.6).
    ///
    /// The Apply glue calls this before writing a dirty monitor edit; a `true` result
    /// means another program edited the file, so the write must be aborted and the
    /// model re-loaded rather than clobbering the stale parse. Always `false` when the
    /// file was unreadable at load (nothing was baselined, and there is nothing to
    /// write).
    pub(crate) fn check_conflict(&self) -> bool {
        !self.freshness.check_conflicts().is_empty()
    }

    /// Re-reads `monitors.conf` and re-probes the compositor, returning a fresh model
    /// with a new freshness baseline (R5.6 "warn and re-load").
    ///
    /// Called after [`Self::check_conflict`] detects an external edit: the fresh model
    /// re-parses the current file (discarding the now-stale staged edits, which were
    /// based on the superseded bytes) so a subsequent Apply builds on the current
    /// contents. Returns `None` if the compositor can no longer be probed.
    pub(crate) fn reload(&self, runner: &dyn CommandRunner) -> Option<Self> {
        Self::load_with(
            runner,
            self.monitors_conf_path.clone(),
            self.state_file.clone(),
        )
    }

    /// Stages a new resolution for monitor `index`, composing it with a refresh rate
    /// valid for that resolution (keeping the current one when still available, else
    /// the first the compositor reports) into the mode field.
    pub(crate) fn stage_resolution(&mut self, index: usize, resolution: String) {
        let refresh = {
            let current = self.effective_refresh(index);
            let available = self.refresh_options_for(index, &resolution);
            match current {
                Some(refresh) if available.contains(&refresh) => Some(refresh),
                _ => available.into_iter().next(),
            }
        };
        self.stage_mode(index, compose_mode(&resolution, refresh.as_deref()));
    }

    /// Stages a new refresh rate for monitor `index`, keeping its current resolution.
    pub(crate) fn stage_refresh(&mut self, index: usize, refresh: String) {
        let resolution = self.effective_resolution(index);
        self.stage_mode(index, compose_mode(&resolution, Some(&refresh)));
    }

    /// Stages a new scale for monitor `index`, rejecting a value outside the plausible
    /// range (R8.3) so the drop-down snaps back rather than writing a broken scale.
    pub(crate) fn stage_scale(&mut self, index: usize, scale: String) {
        match scale.parse::<f64>() {
            Ok(value) if validate_float_range(value, &SCALE_RANGE).is_ok() => {}
            _ => {
                tracing::warn!(scale = %scale, "rejecting an out-of-range monitor scale (R8.3)");
                return;
            }
        }
        self.ensure_staged(index).scale = scale;
        self.clear_if_unchanged(index);
    }

    /// Stages a new position for monitor `index`.
    pub(crate) fn stage_position(&mut self, index: usize, position: String) {
        self.ensure_staged(index).position = position;
        self.clear_if_unchanged(index);
    }

    /// Stages an enable/disable edit for a **non-laptop** monitor `index` (a staged
    /// `monitors.conf` change). A laptop panel must use [`Self::toggle_laptop`]
    /// instead; calling this for one is a no-op guarded by a debug assertion.
    pub(crate) fn stage_enabled(&mut self, index: usize, enabled: bool) {
        debug_assert!(
            !self.monitors[index].is_laptop,
            "a laptop panel's enablement is runtime-only (R5.2); use toggle_laptop"
        );
        if self.monitors[index].is_laptop {
            return;
        }
        self.ensure_staged(index).enabled = enabled;
        self.clear_if_unchanged(index);
    }

    /// Stages the mode field for monitor `index`, rejecting a malformed mode (R8.3).
    fn stage_mode(&mut self, index: usize, mode: String) {
        if validate_monitor_mode(&mode).is_err() {
            tracing::warn!(mode = %mode, "rejecting a malformed monitor mode (R8.3)");
            return;
        }
        self.ensure_staged(index).mode = mode;
        self.clear_if_unchanged(index);
    }

    /// Returns a mutable reference to monitor `index`'s staged config, initialising it
    /// from the original on first edit.
    fn ensure_staged(&mut self, index: usize) -> &mut MonitorConfig {
        let monitor = &mut self.monitors[index];
        monitor
            .staged
            .get_or_insert_with(|| monitor.original.clone())
    }

    /// Drops monitor `index`'s staged config when it has been edited back to equal the
    /// original, so re-selecting the current value never leaves the page dirty.
    fn clear_if_unchanged(&mut self, index: usize) {
        let monitor = &mut self.monitors[index];
        if monitor.staged.as_ref() == Some(&monitor.original) {
            monitor.staged = None;
        }
    }

    /// The refresh-rate options for a specific `resolution` of monitor `index`: the
    /// refreshes the compositor reports for it, plus the configured refresh when the
    /// selected resolution is the configured one and it is not otherwise listed. Empty
    /// for a special mode token.
    fn refresh_options_for(&self, index: usize, resolution: &str) -> Vec<String> {
        if is_special_mode(resolution) {
            return Vec::new();
        }
        let mut options: Vec<String> = Vec::new();
        for mode in &self.monitors[index].available {
            if mode.resolution == resolution && !options.contains(&mode.refresh) {
                options.push(mode.refresh.clone());
            }
        }
        // Keep the configured refresh selectable when it applies to this resolution.
        if self.effective_resolution(index) == resolution {
            if let Some(refresh) = self.effective_refresh(index) {
                if !options.contains(&refresh) {
                    options.insert(0, refresh);
                }
            }
        }
        options
    }

    /// Whether any monitor has a pending file-backed edit — the Display page's dirty
    /// state, which the window folds into the global Apply/Reset chrome (R5.1).
    pub(crate) fn is_dirty(&self) -> bool {
        self.monitors.iter().any(Monitor::is_dirty)
    }

    /// Discards every staged monitor edit, returning the page to clean (R5.1).
    pub(crate) fn reset(&mut self) {
        for monitor in &mut self.monitors {
            monitor.staged = None;
        }
    }

    /// The Display page's contribution to the Apply plan, or `None` when there is
    /// nothing to write (no dirty edit, or `monitors.conf` was unreadable).
    ///
    /// It clones the parsed file, applies each dirty monitor's edits through the
    /// surgical [`monitors`](crate::parsers::monitors) parser (so only the targeted
    /// records change and stay awk-parseable), and emits the complete new bytes as a
    /// [`FileWrite`] for the shared pipeline, alongside the value validations (R8.3).
    /// A parser edit error (which should not occur for validated values) is logged and
    /// yields `None` rather than a partial write.
    pub(crate) fn apply_contribution(&self) -> Option<DisplayApply> {
        if !self.is_dirty() {
            return None;
        }
        let mut file = self.records.clone()?;
        let changed_keys = match apply_edits(&mut file, &self.monitors) {
            Ok(keys) => keys,
            Err(error) => {
                tracing::error!(%error, "failed to render a monitors.conf edit; skipping the display write");
                return None;
            }
        };
        if changed_keys.is_empty() {
            return None;
        }

        Some(DisplayApply {
            write: FileWrite {
                path: self.monitors_conf_path.clone(),
                contents: file.emit().into_bytes(),
                changed_keys,
                backing: BackingFile::MonitorsConf,
            },
            validations: self.staged_validations(),
        })
    }

    /// Commits the staged edits after a successful Apply: applies them to the in-memory
    /// `monitors.conf` and promotes each monitor's staged config to its original, so
    /// the model reflects what was just written and the page is clean again.
    pub(crate) fn commit(&mut self) {
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
    /// contributes its mode and scale to be re-checked by the pipeline before any
    /// write, reusing the [`SettingId`] validators.
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
            if let Ok(scale) = config.scale.parse::<f64>() {
                validations.push((SettingId::MonitorScale, Value::Float(scale)));
            }
        }
        validations
    }

    /// Applies the laptop-display enable toggle immediately (runtime-only, R5.2),
    /// mirroring `scripts/hypr-toggle-laptop-display` (analysis §3/§6.2).
    ///
    /// Turning the panel **on** re-applies its configured record live with
    /// `hyprctl keyword monitor "<output>,<mode>,<position>,<scale>[,<extras>]"` and
    /// **creates** the hotplug override state file; turning it **off** issues
    /// `hyprctl keyword monitor "<output>,disable"` and **removes** the state file. The
    /// granular `hyprctl keyword monitor` command is deliberate: `hyprctl reload` would
    /// re-read the (active) `monitor=` record and so cannot turn the panel off, whereas
    /// the keyword command applies the on/off state directly — this is the exact
    /// mechanism the dotfiles' toggle/hotplug scripts use, so keep it in step with them.
    /// The state file is the manual override the hotplug watcher reads on the next
    /// hotplug event (present = keep the panel on when docked), so the watcher does not
    /// fight the choice.
    ///
    /// This never stages, never becomes dirty, and **never touches `monitors.conf`**
    /// (which would fight the watcher). The live keyword call is best-effort (R5.5): a
    /// failure is logged but does not fail the toggle, since the persistent override is
    /// still recorded. The stored live-enabled state is updated so the switch reflects
    /// the new state.
    pub(crate) fn toggle_laptop(
        &mut self,
        index: usize,
        enable: bool,
        runner: &dyn CommandRunner,
    ) -> std::io::Result<()> {
        let output = self.monitors[index].output_name.clone();
        let command = if enable {
            let body = self.laptop_keyword_body(index);
            Command::new("hyprctl")
                .arg("keyword")
                .arg("monitor")
                .arg(format!("{output},{body}"))
        } else {
            Command::new("hyprctl")
                .arg("keyword")
                .arg("monitor")
                .arg(format!("{output},disable"))
        };

        // Apply live via the granular keyword command; failures are non-fatal (R5.5).
        if let Err(error) = runner.run(&command) {
            tracing::error!(%error, "the laptop-display keyword command failed (R5.5)");
        }

        // Record/clear the hotplug override, matching the toggle script's directions
        // (on ⇒ touch, off ⇒ remove).
        if enable {
            fs::write(&self.state_file, b"")?;
            tracing::info!(
                path = %self.state_file.display(),
                "enabled the laptop display and set the hotplug override (R5.2)"
            );
        } else {
            match fs::remove_file(&self.state_file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            tracing::info!(
                path = %self.state_file.display(),
                "disabled the laptop display and cleared the hotplug override (R5.2)"
            );
        }

        self.monitors[index].live_enabled = enable;
        Ok(())
    }

    /// The `<mode>,<position>,<scale>[,<extras>]` body used to re-enable the laptop
    /// panel live, sourced from its configured `monitor=` record (the single source,
    /// analysis §6.2) so the live re-enable matches the on-disk record — extras and
    /// all. Falls back to the merged config's mode/position/scale when there is no
    /// record body (an unreadable `monitors.conf`, or a record-less output).
    fn laptop_keyword_body(&self, index: usize) -> String {
        if let (Some(records), Some(name)) =
            (&self.records, self.monitors[index].record_name.as_deref())
        {
            if let Some(body) = records.record_body(name) {
                return body.to_string();
            }
        }
        let config = &self.monitors[index].original;
        format!("{},{},{}", config.mode, config.position, config.scale)
    }
}

/// Applies each dirty monitor's staged edits to `file`, returning the changed-key
/// labels for logging (R7.3). Shared by [`DisplayModel::apply_contribution`] (on a
/// clone) and [`DisplayModel::commit`] (in place) so both render edits identically.
///
/// An in-place field edit ([`MonitorField`]) preserves trailing extras such as
/// `bitdepth,10`; a (re-)enable or a first-time configure writes the whole record body
/// with [`MonitorState::Active`]; a disable collapses it to the disable form. A laptop
/// panel's enablement is never staged (it is runtime-only), so this never writes a
/// disable record for it.
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
            if staged.enabled {
                file.set_state(name, &active_state(staged))?;
                changed.push(format!("monitor {} enabled", monitor.output_name));
            } else {
                file.set_state(name, &MonitorState::Disabled)?;
                changed.push(format!("monitor {} disabled", monitor.output_name));
            }
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
                file.set_state(name, &active_state(staged))?;
                changed.push(format!("monitor {} configured", monitor.output_name));
            }
        }
    }
    Ok(changed)
}

/// The [`MonitorState::Active`] form of a config, sourcing mode/position/scale from
/// the (staged) values — used when (re-)enabling or first-configuring an output.
fn active_state(config: &MonitorConfig) -> MonitorState {
    MonitorState::Active {
        mode: config.mode.clone(),
        position: config.position.clone(),
        scale: config.scale.clone(),
    }
}

/// The parser edit error type, re-exported locally for [`apply_edits`]'s signature.
type EditError = crate::parsers::monitors::EditError;

/// A monitor as reported by `hyprctl monitors -j`, before merging with the records.
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

/// Parses the `hyprctl monitors -j` JSON array into [`LiveMonitor`]s, skipping any
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
    let record_name = records.and_then(|file| match_record(file, &live.name, &live.description));

    // Read the configured values from a specific, enabled record; otherwise fall back
    // to the live state so the drop-downs still show something meaningful.
    let record_enabled = record_name
        .as_ref()
        .and_then(|name| records.and_then(|file| file.is_enabled(name)));
    let has_enabled_record = record_enabled == Some(true);

    let live_mode = live_mode_string(&live);
    let live_scale = trim_float(live.scale);
    let live_position = format!("{}x{}", live.position.0, live.position.1);

    let (mode, position, scale) = if has_enabled_record {
        let name = record_name
            .as_deref()
            .expect("an enabled record has a name");
        let file = records.expect("a record implies a records file");
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
    // invariant, analysis §6.2). If that record were instead in `,disable` form,
    // `has_enabled_record` would be false and a mode/scale edit would re-activate the
    // record in `monitors.conf` via `set_state` (see `apply_edits`); that is the
    // intended, if unusual, behaviour, not a bug. A non-laptop output follows its
    // record, falling back to the live `disabled` flag when no record configures it.
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
        record_name,
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
    use crate::system::command::{Command, CommandOutput, MockCommandRunner};

    /// A realistic `hyprctl monitors -j` payload for the personal laptop: one eDP-1
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

    /// Builds a laptop-only model with an injected (nonexistent) state file, from the
    /// canned sources — the merge under test.
    fn laptop_model(dir: &std::path::Path) -> DisplayModel {
        DisplayModel::from_sources(
            Some(MONITORS_CONF),
            HYPRCTL_LAPTOP,
            dir.join("monitors.conf"),
            dir.join("forced"),
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
        assert_eq!(model.monitor_name(0), "eDP-1");
        assert_eq!(model.monitor_description(0), "AU Optronics 0x2036");
        assert!(model.is_laptop(0), "eDP-1 is the internal laptop panel");
        assert!(model.records_editable());

        // Configured values from the winning desc record (not the generic eDP-1 rule).
        assert_eq!(model.effective_resolution(0), "2560x1440");
        assert_eq!(
            model.effective_refresh(0),
            None,
            "the record's mode is a bare WIDTHxHEIGHT, so no refresh is preselected"
        );
        assert_eq!(model.effective_scale(0), "1.066667");
        assert_eq!(model.effective_position(0), "auto");

        // Available modes come from the live probe.
        assert_eq!(model.resolution_options(0), vec!["2560x1440".to_string()]);
        assert_eq!(model.refresh_options(0), vec!["60.01".to_string()]);
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
        let model = DisplayModel::load_with(&runner, path, dir.path().join("forced"))
            .expect("a successful probe yields a model");

        assert_eq!(model.monitor_count(), 1);
        assert_eq!(model.effective_scale(0), "1.066667");
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
        assert!(
            DisplayModel::load_with(
                &runner,
                dir.path().join("monitors.conf"),
                dir.path().join("f")
            )
            .is_none()
        );
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
        let mut model = DisplayModel::load_with(
            &runner,
            dir.path().join("does-not-exist.conf"),
            dir.path().join("forced"),
        )
        .expect("the probe succeeded");

        assert_eq!(model.monitor_count(), 1);
        assert!(
            !model.records_editable(),
            "an unreadable monitors.conf hides the file-backed rows (R4.4)"
        );
        // Even a staged edit produces no write when there is no file to edit.
        model.stage_scale(0, "1.5".to_string());
        assert!(model.apply_contribution().is_none());
    }

    #[test]
    fn staging_a_scale_produces_a_surgical_awk_parseable_write() {
        // Test #2 (the write): staging the laptop's scale rewrites only that record's
        // scale field, leaving the leading name token, the mode, and the position
        // untouched — so the record stays parseable by hypr-display-profile.sh's awk
        // (name field 1, mode field 2, scale field 4).
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());

        model.stage_scale(0, "1.25".to_string());
        assert!(model.is_dirty());

        let contribution = model
            .apply_contribution()
            .expect("a dirty edit produces a write");
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

        // The validations re-check the mode and scale (R8.3).
        assert!(
            contribution
                .validations
                .contains(&(SettingId::MonitorScale, Value::Float(1.25)))
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
        let mut model = DisplayModel::from_sources(
            Some(conf),
            hyprctl,
            dir.path().join("monitors.conf"),
            dir.path().join("forced"),
        );

        model.stage_resolution(0, "1920x1200".to_string());
        let contribution = model.apply_contribution().expect("a write");
        let emitted = String::from_utf8(contribution.write.contents).expect("utf-8");
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
        model.stage_scale(0, "1.25".to_string());
        assert!(model.is_dirty());
        model.stage_scale(0, "1.066667".to_string());
        assert!(
            !model.is_dirty(),
            "editing back to the original clears dirty"
        );
        assert!(model.apply_contribution().is_none());
    }

    #[test]
    fn reset_discards_staged_edits() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        model.stage_scale(0, "1.5".to_string());
        assert!(model.is_dirty());
        model.reset();
        assert!(!model.is_dirty());
        assert_eq!(model.effective_scale(0), "1.066667");
    }

    #[test]
    fn commit_promotes_staged_and_updates_the_records() {
        // After a successful apply, commit makes the staged value the new baseline and
        // keeps the in-memory records in step, so a further edit builds on it.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        model.stage_scale(0, "1.5".to_string());
        model.commit();
        assert!(!model.is_dirty(), "commit clears dirty");
        assert_eq!(
            model.effective_scale(0),
            "1.5",
            "the staged value is the new original"
        );

        // A subsequent write renders from the committed records: only the scale differs
        // from the committed state.
        model.stage_position(0, "0x0".to_string());
        let emitted =
            String::from_utf8(model.apply_contribution().expect("write").write.contents).unwrap();
        assert!(emitted.contains("monitor=desc:AU Optronics 0x2036,2560x1440,0x0,1.5"));
    }

    #[test]
    fn an_out_of_range_scale_is_rejected() {
        // R8.3: an implausible scale is refused on stage, so it never reaches a write.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = laptop_model(dir.path());
        model.stage_scale(0, "9".to_string());
        assert!(!model.is_dirty(), "an out-of-range scale must not stage");
    }

    #[test]
    fn disabling_a_non_laptop_monitor_writes_the_disable_form() {
        // A non-laptop output's enable switch stages a monitors.conf edit. Disabling it
        // collapses its record to the disable form.
        let dir = tempfile::tempdir().expect("temp dir");
        let hyprctl = r#"[{"name":"DP-1","description":"Lenovo Group Limited P24q-10 U4P00001","width":2560,"height":1440,"refreshRate":60.0,"x":0,"y":0,"scale":1.0,"disabled":false,"availableModes":["2560x1440@60.00Hz"]}]"#;
        let mut model = DisplayModel::from_sources(
            Some(MONITORS_CONF),
            hyprctl,
            dir.path().join("monitors.conf"),
            dir.path().join("forced"),
        );
        assert!(!model.is_laptop(0));
        assert!(model.effective_enabled(0));

        model.stage_enabled(0, false);
        let emitted =
            String::from_utf8(model.apply_contribution().expect("write").write.contents).unwrap();
        assert!(
            emitted.contains("monitor=desc:Lenovo Group Limited P24q-10 U4P00001,disable"),
            "the external monitor's record collapses to the disable form:\n{emitted}"
        );
    }

    #[test]
    fn laptop_toggle_uses_hyprctl_keyword_and_the_state_file_without_touching_monitors_conf() {
        // Test #3 (F1): the runtime laptop toggle mirrors hypr-toggle-laptop-display —
        // it applies the on/off state with the granular `hyprctl keyword monitor`
        // command (NOT `hyprctl reload`, which cannot disable the panel), creates the
        // state file on enable and removes it on disable, and never writes monitors.conf.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf_path = dir.path().join("monitors.conf");
        std::fs::write(&conf_path, MONITORS_CONF).expect("write monitors.conf");
        let state_file = dir.path().join("forced");
        let mut model = DisplayModel::from_sources(
            Some(MONITORS_CONF),
            HYPRCTL_LAPTOP,
            conf_path.clone(),
            state_file.clone(),
        );
        assert!(model.is_laptop(0));
        assert!(
            model.laptop_enabled(0),
            "the live probe reports the panel on, so the switch starts on"
        );
        assert!(!state_file.exists());

        let runner = MockCommandRunner::new();

        // Turn OFF: `hyprctl keyword monitor eDP-1,disable` + remove the state file.
        model
            .toggle_laptop(0, false, &runner)
            .expect("toggle off succeeds");
        assert!(
            !model.laptop_enabled(0),
            "the switch reflects the panel now off"
        );
        assert!(!state_file.exists(), "disable removes the state file");

        // Turn ON: re-apply the record body live + create the state file. The body is
        // sourced from the winning eDP desc record (2560x1440,auto,1.066667).
        model
            .toggle_laptop(0, true, &runner)
            .expect("toggle on succeeds");
        assert!(
            model.laptop_enabled(0),
            "the switch reflects the panel now on"
        );
        assert!(state_file.exists(), "enable creates the state file");

        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl").args(["keyword", "monitor", "eDP-1,disable"]),
                Command::new("hyprctl").args([
                    "keyword",
                    "monitor",
                    "eDP-1,2560x1440,auto,1.066667",
                ]),
            ],
            "the toggle applies via the granular keyword command, never hyprctl reload"
        );
        // The toggle is runtime-only: monitors.conf on disk is byte-for-byte unchanged.
        assert_eq!(
            std::fs::read_to_string(&conf_path).expect("read monitors.conf"),
            MONITORS_CONF,
            "the laptop toggle must never touch monitors.conf"
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
        let mut model = DisplayModel::load_with(&runner, conf_path.clone(), dir.path().join("f"))
            .expect("a successful probe yields a model");

        assert!(
            !model.check_conflict(),
            "an untouched file is not a conflict"
        );
        model.stage_scale(0, "1.25".to_string());

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
        let mut model = DisplayModel::load_with(&runner, conf_path.clone(), dir.path().join("f"))
            .expect("the probe succeeded");

        model.stage_scale(0, "1.25".to_string());
        let contribution = model
            .apply_contribution()
            .expect("a dirty edit produces a write");
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
        let mut model = DisplayModel::from_sources(
            Some(conf),
            hyprctl,
            dir.path().join("monitors.conf"),
            dir.path().join("forced"),
        );
        assert_eq!(model.effective_resolution(0), "3840x2160");
        assert_eq!(model.effective_refresh(0), Some("60".to_string()));

        // 60 is not offered for 2560x1440 (which offers 144 and 120), so the first
        // available (144) is chosen.
        model.stage_resolution(0, "2560x1440".to_string());
        assert_eq!(model.effective_resolution(0), "2560x1440");
        assert_eq!(model.effective_refresh(0), Some("144".to_string()));
        assert_eq!(
            model.refresh_options(0),
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
            dir.path().join("forced"),
        );
        assert_eq!(model.monitor_count(), 0);
        assert!(!model.is_dirty());
    }

    #[test]
    fn scale_and_position_option_lists_include_the_configured_value() {
        let dir = tempfile::tempdir().expect("temp dir");
        let model = laptop_model(dir.path());
        // The configured scale is first (preselected), followed by the curated set.
        let scales = model.scale_options(0);
        assert_eq!(scales[0], "1.066667");
        assert!(scales.contains(&"1.333333".to_string()));
        // The configured position ("auto") is present; the curated set adds "0x0".
        let positions = model.position_options(0);
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
        let mut model = DisplayModel::from_sources(
            Some(conf),
            hyprctl,
            dir.path().join("monitors.conf"),
            dir.path().join("forced"),
        );

        // The baseline comes from the live state (no record to read).
        assert_eq!(model.effective_resolution(0), "1920x1080");
        assert_eq!(model.effective_refresh(0), Some("60".to_string()));
        assert_eq!(model.effective_scale(0), "1");
        assert!(model.effective_enabled(0));

        // Editing appends a specific DP-5 record after the last monitor= line, leaving
        // the catch-all and eDP lines untouched.
        model.stage_scale(0, "1.25".to_string());
        let emitted =
            String::from_utf8(model.apply_contribution().expect("write").write.contents).unwrap();
        assert_eq!(
            emitted,
            "monitor=,preferred,auto,1\nmonitor=eDP-1,2880x1800@120,auto,1.333333\nmonitor=DP-5,1920x1080@60,0x0,1.25\n",
            "a first-configure appends a specific record sourced from the live state"
        );
    }

    #[test]
    fn re_enabling_a_disabled_monitor_sources_mode_and_scale_from_live() {
        // F3(b): a monitor whose record is in the disable form takes its baseline from
        // the live `hyprctl` state, and re-enabling it writes a full record with those
        // live-sourced mode/position/scale via set_state.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf = "monitor=desc:Ext Vendor Model,disable\nmonitor=,preferred,auto,1\n";
        // The disabled output still reports its last-known mode via `monitors all -j`.
        let hyprctl = r#"[{"name":"DP-2","description":"Ext Vendor Model","width":3440,"height":1440,"refreshRate":100.0,"x":0,"y":0,"scale":1.0,"disabled":true,"availableModes":["3440x1440@100.00Hz"]}]"#;
        let mut model = DisplayModel::from_sources(
            Some(conf),
            hyprctl,
            dir.path().join("monitors.conf"),
            dir.path().join("forced"),
        );
        assert!(!model.is_laptop(0));
        assert!(
            !model.effective_enabled(0),
            "the record disable form reads as off"
        );
        // The mode/scale baseline is the live state, since the disabled record exposes
        // no fields to read.
        assert_eq!(model.effective_resolution(0), "3440x1440");
        assert_eq!(model.effective_scale(0), "1");

        // Re-enabling writes a full record sourced from those live values.
        model.stage_enabled(0, true);
        let emitted =
            String::from_utf8(model.apply_contribution().expect("write").write.contents).unwrap();
        assert!(
            emitted.contains("monitor=desc:Ext Vendor Model,3440x1440@100,0x0,1"),
            "re-enable writes the live-sourced mode/position/scale via set_state:\n{emitted}"
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
        let mut model = DisplayModel::from_sources(
            Some(conf),
            hyprctl,
            dir.path().join("monitors.conf"),
            dir.path().join("forced"),
        );

        // First edit: appends a specific DP-7 record, then commit.
        model.stage_scale(0, "1.25".to_string());
        model.commit();

        // Second edit: must edit the appended record in place, so there is exactly one
        // DP-7 record (no duplicate append).
        model.stage_scale(0, "1.5".to_string());
        let emitted =
            String::from_utf8(model.apply_contribution().expect("write").write.contents).unwrap();
        assert_eq!(
            emitted.matches("monitor=DP-7,").count(),
            1,
            "the second edit edits the appended record in place, not appends again:\n{emitted}"
        );
        assert!(emitted.contains("monitor=DP-7,1920x1080@60,0x0,1.5"));
    }
}
