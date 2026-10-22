//! The staging state machine: `original`/`staged` values per setting, dirty
//! tracking, and external-edit conflict handling (task 4.2; architecture §6
//! "Staging"; R5.1, R5.2, R5.6, R6.2).
//!
//! # What this module is
//!
//! [`SettingsStore`] is the in-memory model behind the whole UI. For every
//! file-backed setting it holds an `original` [`Value`] (as read from disk) and an
//! optional `staged` [`Value`] (the user's pending edit), keyed by [`SettingId`].
//! A setting is **dirty** when it has a staged value that differs from its original;
//! the store rolls that up to a whole-page ([`Category`]) marker and a global "any
//! dirty" flag that drive the Apply/Reset chrome (task 5.3). Nothing here touches
//! GTK — the store is `core/` domain logic so the staging behaviour is unit-tested
//! headlessly (R6.2), and the layering guard in `tests/module_boundaries.rs` forbids
//! any `gtk`/`relm4` import.
//!
//! # The lifecycle
//!
//! 1. **Load.** Startup (task 5.4) reads and parses each backing file once, then
//!    calls [`SettingsStore::load_file`] with the parsed originals, the exact bytes
//!    it read, and a *reader* closure that knows how to re-read and re-parse that
//!    file later. The store fingerprints those same bytes as the freshness baseline
//!    (see below).
//! 2. **Stage.** The UI emits edits into [`SettingsStore::stage`]. A file-backed
//!    edit is validated and recorded as the staged value; a runtime-only edit is
//!    reported back as a bypass so the caller applies it immediately (R5.2).
//! 3. **Reset.** [`SettingsStore::reset`] discards every staged value, returning the
//!    store to clean (R5.1).
//! 4. **Refresh.** On window focus / manual refresh, [`SettingsStore::refresh`]
//!    re-reads the tracked files; any that changed externally have their originals
//!    reloaded from disk so a later Apply does not silently clobber those edits
//!    (R5.6).
//! 5. **Commit.** After the Apply pipeline ([`crate::core::apply`]) reports a
//!    successful apply, the caller must call [`SettingsStore::commit_apply`], which
//!    promotes the staged values to originals and re-baselines the written files'
//!    freshness; its docs state that contract and why the pipeline cannot do it.
//!
//! # Why the freshness baseline is taken from the store's own bytes
//!
//! Conflict detection lives in [`FreshnessTracker`], which the store owns. Crucially
//! the store records the baseline with [`FreshnessTracker::record_bytes`] — the
//! *exact bytes it parsed the original from* — rather than [`FreshnessTracker::record`],
//! which would re-read the file. Re-reading would both double the IO and open a
//! time-of-check/time-of-use window: an external edit landing between the store's
//! read and a tracker re-read would be baselined as "original", so the later write
//! would clobber it undetected. Fingerprinting the same bytes keeps the baseline and
//! the in-memory `original` in lockstep, so that race surfaces as a conflict instead
//! (see [`FreshnessTracker::record_bytes`] for the full argument).
//!
//! # Runtime-only settings bypass staging (R5.2)
//!
//! Volume/mute and the laptop-display toggle touch no config file and take effect
//! immediately, so they are never staged and never dirty. The store recognises them
//! by their [`SettingId::backing`] marker: [`SettingsStore::stage`] returns
//! [`StageOutcome::RuntimeBypass`] for such a setting and stores nothing, leaving the
//! caller to run the system command. The store's map therefore only ever contains
//! file-backed settings, so dirty tracking, the per-page rollup, and reset all
//! exclude runtime-only settings automatically.
//!
//! # Linked settings: two views of one on-disk value
//!
//! Normally each [`SettingId`] owns its own key in its file. One pair does not: the
//! hyprlock grace period ([`SettingId::LockGracePeriod`], task 11.13) is the `--grace N`
//! token *inside* the lock command ([`SettingId::LockCommand`], hypridle's
//! `general.lock_cmd`), because hyprlock ≥ 0.9 reads it only from its command line. If
//! the two were staged independently they would drift apart: a grace-only Apply would
//! leave the lock-command field showing the old `--grace` text (and a later edit of that
//! field would silently write the old number back), and a lock command typed with a new
//! `--grace` would leave the grace control showing the old value. So
//! [`SettingsStore::stage`] keeps them in step, as one rule applied at the only place
//! values enter the store:
//!
//! - staging a grace period also stages the lock command rewritten with that grace token
//!   ([`set_grace`] on the lock command's current value); if that command offers no
//!   editable grace token the grace edit is refused (R8.3) and nothing is staged;
//! - staging a lock command also stages the grace period it now carries
//!   ([`parse_grace`]; no flag reads as `0`), or — if the new command offers no editable
//!   grace token — discards any staged grace edit, so a grace number can never be
//!   applied against a command it no longer fits.
//!
//! Both halves are computed before anything is recorded, so a refused edit leaves the
//! store exactly as it was. Because both settings are always staged together, Reset,
//! commit and dirty tracking need no special case: the pair is dirty, discarded and
//! promoted as one.
//!
//! # Dirty comparison and `NaN` (validate-on-stage)
//!
//! [`Value`] derives [`PartialEq`] but not [`Eq`] because it can hold an [`f64`], and
//! `NaN != NaN`. A `NaN` sitting in a `staged` value would compare unequal to *any*
//! original — including an identical `NaN` — so the setting would read as
//! permanently dirty and even survive [`reset`](SettingsStore::reset) if reset merely
//! copied values around. The store closes this off by **validating every value on
//! the way in**: [`SettingsStore::stage`] runs [`SettingId::validate`] first and
//! rejects the edit if it fails, and the range validators reject a non-finite float
//! (`NaN`/infinity is outside every finite range). A `NaN` therefore never enters
//! `staged`. Reset is independently safe regardless of originals because it clears
//! `staged` to `None` (dirty is `false` whenever there is no staged value), so it can
//! never leave a value stuck dirty.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::core::freshness::{ConflictReason, FreshnessTracker};
use crate::core::lock_grace::{parse_grace, set_grace};
use crate::core::model::{Backing, Category, SettingId, ValidationError, Value};

/// The effect a staged value has on the setting linked to it (see the module docs on
/// linked settings): `Some(value)` stages `value` for that setting, `None` discards its
/// staged edit.
type LinkedEdit = (SettingId, Option<Value>);

/// The freshly-read contents of one backing file: the raw bytes plus the original
/// values parsed from them.
///
/// This is what a [`FileReader`] returns and what [`SettingsStore::load_file`] is
/// given for the initial load. Bundling the bytes with the values guarantees the two
/// come from a *single* read of the file, which is what lets the store baseline the
/// freshness tracker against the exact bytes it parsed (see the module docs).
#[derive(Debug)]
pub struct FileValues {
    /// The file's exact bytes at the moment it was read, used verbatim as the
    /// freshness baseline.
    pub bytes: Vec<u8>,
    /// The original values parsed from those bytes, each keyed by the setting it
    /// backs. A runtime-only [`SettingId`] appearing here is ignored (the store only
    /// tracks file-backed settings).
    pub values: Vec<(SettingId, Value)>,
}

/// A closure that re-reads and re-parses a backing file into its current
/// [`FileValues`].
///
/// The store keeps one per loaded file so that [`SettingsStore::refresh`] can reload
/// the originals of a file that changed on disk (R5.6) without the store itself
/// depending on any parser — the closure, supplied by the loader (task 5.4),
/// encapsulates "read these bytes and turn them into `(SettingId, Value)` pairs". It
/// is [`Send`] so the store can be built on the startup worker thread and moved to
/// the UI thread (architecture §8). It is boxed behind this alias so the store's
/// method signatures stay readable.
pub type FileReader = Box<dyn Fn(&Path) -> io::Result<FileValues> + Send>;

/// The outcome of a successful [`SettingsStore::stage`] call.
///
/// Tells the caller whether the value was recorded as a pending file-backed edit or
/// must be applied immediately because the setting is runtime-only (R5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageOutcome {
    /// The value was recorded as a pending edit to a config file (R5.1). It will be
    /// written on Apply and shows as dirty until then (unless it equals the
    /// original).
    Staged,
    /// The setting is runtime-only (R5.2): nothing was staged, and the caller must
    /// apply the value to the live session itself (e.g. via a `wpctl` command or the
    /// laptop-display hotplug mechanism). The store holds no state for it.
    RuntimeBypass,
}

/// Why a [`SettingsStore::stage`] call was refused.
#[derive(Debug)]
pub enum StageError {
    /// The proposed value failed validation (R8.3); nothing was staged. This is the
    /// guard that also keeps a `NaN` float out of `staged` (see the module docs).
    Invalid(ValidationError),
    /// The setting is file-backed but was never loaded into the store, so there is no
    /// original to stage against. This is a programming error — staging a setting the
    /// loader never registered — surfaced as an error rather than a panic.
    NotLoaded(SettingId),
}

impl From<ValidationError> for StageError {
    fn from(error: ValidationError) -> Self {
        StageError::Invalid(error)
    }
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageError::Invalid(error) => write!(f, "{error}"),
            StageError::NotLoaded(id) => {
                write!(f, "setting {id:?} is not loaded, so it cannot be staged")
            }
        }
    }
}

impl std::error::Error for StageError {}

/// The result of a [`SettingsStore::refresh`]: which tracked files changed on disk
/// and how the store responded (R5.6).
///
/// The UI (task 5.3) uses this to warn the user that files were edited externally
/// and to say which reloaded cleanly versus which could not be re-read.
#[derive(Debug, Default)]
pub struct RefreshReport {
    /// Files that changed externally and whose originals were successfully reloaded
    /// from disk (and re-baselined for future conflict checks).
    reloaded: Vec<PathBuf>,
    /// Files that changed externally but could not be reloaded — deleted, no longer
    /// readable, or no longer parseable. Their in-memory originals are left as the
    /// last known good values so the app can still function.
    failed: Vec<PathBuf>,
}

impl RefreshReport {
    /// Whether no external changes were detected at all — the common, quiet case.
    pub fn is_empty(&self) -> bool {
        self.reloaded.is_empty() && self.failed.is_empty()
    }

    /// The files whose originals were reloaded from disk.
    pub fn reloaded(&self) -> &[PathBuf] {
        &self.reloaded
    }

    /// The files that changed but could not be reloaded.
    pub fn failed(&self) -> &[PathBuf] {
        &self.failed
    }
}

/// One tracked setting's `original`/`staged` pair.
#[derive(Clone, Debug)]
struct Entry {
    /// The value as last read from (or reloaded from) disk — the baseline dirty is
    /// measured against.
    original: Value,
    /// The user's pending edit, or `None` when the setting is unedited. Held as an
    /// `Option` so "unedited" is distinct from "staged back to the original value",
    /// and so [`SettingsStore::reset`] can clear it unconditionally.
    staged: Option<Value>,
}

impl Entry {
    /// The value the UI should render: the staged edit if present, else the original.
    fn effective(&self) -> &Value {
        self.staged.as_ref().unwrap_or(&self.original)
    }

    /// Whether this setting has a pending edit that differs from its original.
    ///
    /// A staged value equal to the original is *not* dirty, so staging a value and
    /// then staging the original back clears the dirty state without a reset.
    fn is_dirty(&self) -> bool {
        self.staged
            .as_ref()
            .is_some_and(|staged| staged != &self.original)
    }
}

/// The in-memory staging state machine for all file-backed settings (R5.1).
///
/// See the module documentation for the full model. Only file-backed settings are
/// ever stored here; runtime-only settings bypass staging (R5.2).
pub struct SettingsStore {
    /// The `original`/`staged` pair for every loaded file-backed setting.
    ///
    /// A [`BTreeMap`] so iteration (dirty rollup, logs, tests) is deterministic by
    /// [`SettingId`] order, at no meaningful cost for the handful of settings.
    settings: BTreeMap<SettingId, Entry>,
    /// A reader per loaded backing file, used to reload originals on conflict (R5.6).
    reloaders: BTreeMap<PathBuf, FileReader>,
    /// Freshness baselines for the tracked backing files, used to detect external
    /// edits (R5.6). Baselined from the exact bytes the store parsed (see module
    /// docs).
    freshness: FreshnessTracker,
}

/// Equivalent to [`SettingsStore::new`]: an empty store with no loaded settings.
impl Default for SettingsStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsStore {
    /// Creates an empty store with no loaded settings.
    pub fn new() -> Self {
        Self {
            settings: BTreeMap::new(),
            reloaders: BTreeMap::new(),
            freshness: FreshnessTracker::new(),
        }
    }

    /// Loads the originals of one backing file and registers how to reload it.
    ///
    /// `path` is the live XDG path the file was read from; `initial` carries the
    /// bytes read and the values parsed from them; `reader` re-reads and re-parses
    /// that file for a later conflict reload. The store records `initial.bytes` as
    /// the freshness baseline via [`FreshnessTracker::record_bytes`] (never a second
    /// read — see the module docs) and sets each setting's `original` to its parsed
    /// value with no staged edit.
    ///
    /// Runtime-only settings offered in `initial.values` are ignored with a debug log
    /// rather than stored, upholding the invariant that the store holds only
    /// file-backed settings. The caller is expected to have already read and parsed
    /// the file successfully; a read/parse failure is handled before this point by
    /// hiding the affected controls (R4.4), which is why loading itself is
    /// infallible.
    pub fn load_file(&mut self, path: impl Into<PathBuf>, initial: FileValues, reader: FileReader) {
        let path = path.into();
        self.ingest(&path, initial);
        self.reloaders.insert(path, reader);
    }

    /// Records `values` bytes as the freshness baseline for `path` and folds its
    /// parsed values into the store's originals.
    ///
    /// Shared by the initial [`load_file`](Self::load_file) and the conflict
    /// [`reload_file`](Self::reload_file) so both baseline and ingest a file the same
    /// way. On a first load a setting is inserted with no staged edit; on a reload an
    /// existing setting keeps its pending staged edit while its `original` is refreshed
    /// — so an external change updates the baseline without discarding the user's
    /// in-progress work (that decision is surfaced to the user by the refresh report,
    /// not silently applied).
    ///
    /// Only the ids *present* in `values` are updated: this folds the fresh values in
    /// rather than replacing the whole store, so a setting absent from a reload (e.g.
    /// a key an external edit deleted) keeps its prior `original`. That is a safe
    /// default — the app retains a usable last-known value and would simply re-add the
    /// key on Apply — rather than dropping the setting from the UI mid-session.
    fn ingest(&mut self, path: &Path, values: FileValues) {
        self.freshness.record_bytes(path, &values.bytes);
        // R7.3: the parsed values belong in the journal at `debug` — each typed
        // (setting, value) pair the file yielded. This is the one choke point
        // every load path shares (initial load and conflict reload alike), and
        // it logs a handful of parsed settings, never the file's raw contents.
        // Logged before the loop below filters runtime-only pairs out, so the
        // message says "offered": a runtime-only pair may appear here yet never
        // enter the store (its skip is logged separately).
        tracing::debug!(
            path = %path.display(),
            values = ?values.values,
            "parsed values offered to the store"
        );
        for (id, value) in values.values {
            if id.backing() != Backing::FileBacked {
                tracing::debug!(?id, "ignoring runtime-only setting offered to the store");
                continue;
            }
            match self.settings.get_mut(&id) {
                Some(entry) => entry.original = value,
                None => {
                    self.settings.insert(
                        id,
                        Entry {
                            original: value,
                            staged: None,
                        },
                    );
                }
            }
        }
    }

    /// Stages an edit, or reports that it must be applied immediately (R5.1/R5.2).
    ///
    /// The value is validated first (R8.3): an invalid value — including a `NaN`
    /// float, which every range validator rejects — is refused with
    /// [`StageError::Invalid`] and nothing is recorded, so a bad value can never sit
    /// in `staged` and skew dirty state (see the module docs). A runtime-only setting
    /// then short-circuits to [`StageOutcome::RuntimeBypass`] without being stored. A
    /// file-backed setting's staged value is updated and reported as
    /// [`StageOutcome::Staged`]; staging a setting that was never loaded is a
    /// [`StageError::NotLoaded`] guard rather than a silent insert.
    ///
    /// Staging the lock command or its grace period also updates the other one, and a
    /// grace period the lock command cannot carry is refused; see the module docs on
    /// linked settings (task 11.13).
    pub fn stage(&mut self, id: SettingId, value: Value) -> Result<StageOutcome, StageError> {
        id.validate(&value)?;

        if id.backing() == Backing::RuntimeOnly {
            // Runtime-only (R5.2): applied immediately by the caller, never staged.
            tracing::debug!(?id, "runtime-only edit bypasses staging");
            return Ok(StageOutcome::RuntimeBypass);
        }
        if !self.settings.contains_key(&id) {
            return Err(StageError::NotLoaded(id));
        }

        // Worked out before anything is recorded, so a refusal stages nothing.
        let linked = self.linked_edit(id, &value)?;
        if let Some((linked_id, linked_value)) = linked {
            if let Some(entry) = self.settings.get_mut(&linked_id) {
                tracing::debug!(
                    ?id,
                    ?linked_id,
                    staged = ?linked_value,
                    "kept a linked setting in step"
                );
                entry.staged = linked_value;
            }
        }

        match self.settings.get_mut(&id) {
            Some(entry) => {
                entry.staged = Some(value);
                // R7.3: the staged diff belongs in the journal at `debug` — the
                // on-disk baseline next to the pending value, so an Apply cycle's
                // log shows *what* each edit changed, not merely that one happened.
                // Settings values are non-sensitive (theme names, timeouts, image
                // paths), so logging them at `debug` is within the R7.3 rules.
                tracing::debug!(
                    ?id,
                    original = ?entry.original,
                    staged = ?entry.staged,
                    dirty = entry.is_dirty(),
                    "staged edit"
                );
                Ok(StageOutcome::Staged)
            }
            None => Err(StageError::NotLoaded(id)),
        }
    }

    /// The edit staging `value` for `id` implies for the setting linked to it, if any
    /// (see the module docs on linked settings).
    ///
    /// **This is the single extension point for linked settings.** Any future pair of
    /// settings that are two views of one on-disk key gets its rule as another arm of the
    /// `match` below; every other setting falls through to `_ => Ok(None)` and is staged
    /// on its own. Keeping the rule here, at the one place values enter the store, is
    /// what lets Reset, commit, refresh and dirty tracking treat a linked pair like any
    /// two settings.
    ///
    /// Fails — so the whole stage is refused — when a grace period cannot be written into
    /// the current lock command, or when the rewritten counterpart would itself be
    /// invalid (a grace period typed into the lock command beyond
    /// [`LOCK_GRACE_SECONDS_RANGE`](crate::core::model::LOCK_GRACE_SECONDS_RANGE), R8.3).
    ///
    /// # The out-of-range grace already on disk
    ///
    /// The loader stores what the file says, unvalidated (the store validates only staged
    /// edits), so a hand-written `--grace 3600` loads verbatim as the grace original. The
    /// Power & Idle spin button shows it clamped to the 600-second maximum, but nothing is
    /// staged until the user touches a control. Then the range applies to both views:
    /// every lock-command edit that keeps the `--grace 3600` token is refused with
    /// [`ValidationError::OutOfRange`] (the entry snaps back), until the user lowers the
    /// number, by editing the token in the entry or by moving the spin button, which stages
    /// an in-range value and rewrites the token with it. This is deliberate: the app never
    /// writes a grace beyond the range (R8.3), even one it did not choose.
    fn linked_edit(&self, id: SettingId, value: &Value) -> Result<Option<LinkedEdit>, StageError> {
        match (id, value) {
            (SettingId::LockGracePeriod, Value::Integer(seconds)) => {
                let command = self
                    .value(SettingId::LockCommand)
                    .and_then(Value::as_str)
                    .ok_or(StageError::NotLoaded(SettingId::LockCommand))?;
                // Already range-checked by `validate`; the conversion only guards the type.
                let seconds = u32::try_from(*seconds).map_err(|_| {
                    StageError::Invalid(ValidationError::OutOfRange {
                        value: seconds.to_string(),
                        min: "0".to_string(),
                        max: u32::MAX.to_string(),
                    })
                })?;
                let command = set_grace(command, seconds).map_err(|error| {
                    StageError::Invalid(ValidationError::LockGraceUnavailable(error))
                })?;
                let command = Value::String(command);
                SettingId::LockCommand.validate(&command)?;
                Ok(Some((SettingId::LockCommand, Some(command))))
            }
            (SettingId::LockCommand, Value::String(command))
                if self.settings.contains_key(&SettingId::LockGracePeriod) =>
            {
                match parse_grace(command) {
                    Ok(seconds) => {
                        let grace = Value::Integer(i64::from(seconds.unwrap_or(0)));
                        SettingId::LockGracePeriod.validate(&grace)?;
                        Ok(Some((SettingId::LockGracePeriod, Some(grace))))
                    }
                    // The new command has no editable grace token (hyprlock removed, or
                    // written in a refused shape): drop any pending grace edit rather than
                    // apply it against a command it no longer fits.
                    Err(_) => Ok(Some((SettingId::LockGracePeriod, None))),
                }
            }
            _ => Ok(None),
        }
    }

    /// Discards every staged edit, returning the store to a clean state (R5.1).
    pub fn reset(&mut self) {
        for entry in self.settings.values_mut() {
            entry.staged = None;
        }
        tracing::debug!("discarded all staged edits");
    }

    /// Commits a successful Apply: promotes every pending staged edit to its
    /// `original` and re-baselines the written files' freshness (architecture §6;
    /// task 4.5).
    ///
    /// This is the canonical statement of the commit-after-Applied contract. The
    /// Apply pipeline ([`crate::core::apply::run`]) is a pure orchestrator that never
    /// touches the store, so the store does not learn on its own that its staged
    /// values reached disk. After every
    /// [`ApplyOutcome::Applied`](crate::core::apply::ApplyOutcome::Applied) — and only
    /// then; every other outcome wrote nothing the store should adopt — the caller
    /// (the window, task 5.3) **must** call this, or the store is left out of step
    /// with disk. It does two things:
    ///
    /// 1. **Promote staged → original.** An `Applied` outcome means every staged
    ///    value reached disk, so each becomes the new baseline and the store is clean
    ///    again (dirty is `false`). Every pending edit is promoted, since the caller
    ///    must have included every dirty setting in the applied plan (the apply
    ///    contract); a staged value equal to its original is harmlessly promoted too.
    /// 2. **Re-baseline the written files.** The app just rewrote those files, so
    ///    their on-disk bytes no longer match the baseline recorded when they were
    ///    loaded. Without a new baseline the **next** Apply's conflict check (its
    ///    step 2) would hash the app's own bytes against the stale one, see a
    ///    mismatch, and abort as
    ///    [`ApplyOutcome::Conflicted`](crate::core::apply::ApplyOutcome::Conflicted)
    ///    — a spurious self-conflict. `written` pairs each written file's **live**
    ///    path (the same key the store tracked it under — freshness is keyed by the
    ///    live XDG path, not the symlink-resolved target) with the exact bytes
    ///    written. Fingerprinting those bytes with [`FreshnessTracker::record_bytes`]
    ///    — rather than re-reading the file — also avoids the read→baseline TOCTOU
    ///    window `record_bytes` exists for (an edit landing between the write and a
    ///    re-read would otherwise be silently baselined as ours). A file absent from
    ///    `written` keeps its prior baseline.
    ///
    /// The contract lives here, with the state it reconciles, rather than in the
    /// pipeline: keeping the pipeline free of the store is what lets it run against
    /// a borrowed [`FreshnessTracker`] and be tested on its own, and it loses nothing
    /// because the `Applied` outcome reports exactly which live paths were written.
    /// `written` must list only files this store loaded
    /// ([`AssembledPlan::store_writes`](crate::core::assemble::AssembledPlan::store_writes)
    /// is exactly that list): files a bespoke page model reads itself are re-baselined
    /// by that model's own commit, which
    /// [`ModelCommits`](crate::core::assemble::ModelCommits) selects.
    ///
    /// This never fails: it only mutates in-memory state.
    pub fn commit_apply(&mut self, written: &[(PathBuf, Vec<u8>)]) {
        for entry in self.settings.values_mut() {
            if let Some(staged) = entry.staged.take() {
                entry.original = staged;
            }
        }
        for (path, bytes) in written {
            self.freshness.record_bytes(path, bytes);
        }
        tracing::debug!(
            files = written.len(),
            "committed applied edits: promoted staged values and re-baselined written files"
        );
    }

    /// Whether any file-backed setting has a pending edit differing from its original.
    ///
    /// Drives the suggested-action Apply button (R5.1).
    pub fn is_dirty(&self) -> bool {
        self.settings.values().any(Entry::is_dirty)
    }

    /// Whether any setting on `category`'s page is dirty — the per-page rollup that
    /// drives the sidebar's modified-dot markers (R5.1).
    pub fn is_category_dirty(&self, category: Category) -> bool {
        self.settings
            .iter()
            .any(|(id, entry)| id.category() == category && entry.is_dirty())
    }

    /// The ids of all currently dirty settings, in [`SettingId`] order.
    ///
    /// Useful for the Apply pipeline (task 4.5), which writes only the changed
    /// settings, and for asserting the rollup in tests.
    pub fn dirty_ids(&self) -> Vec<SettingId> {
        self.settings
            .iter()
            .filter(|(_, entry)| entry.is_dirty())
            .map(|(id, _)| *id)
            .collect()
    }

    /// The dirty settings of one page, each paired with its effective (staged) value,
    /// in [`SettingId`] order.
    ///
    /// This is the reusable seam a category page's Apply glue uses to turn its staged
    /// edits into a file write (task 6.6, reused by task 6.7): it hands the page exactly
    /// the settings it must serialise into its backing file — the dirty ones in its
    /// [`Category`] — with their current values, so the page renders each through its
    /// format parser (§3) into one [`FileWrite`](crate::core::apply::FileWrite). A
    /// clean page yields an empty vector, so no write is produced. The returned values
    /// are cloned so the caller is free of the store's borrow while it builds the write.
    pub fn dirty_in_category(&self, category: Category) -> Vec<(SettingId, Value)> {
        self.settings
            .iter()
            .filter(|(id, entry)| id.category() == category && entry.is_dirty())
            .map(|(id, entry)| (*id, entry.effective().clone()))
            .collect()
    }

    /// The value the UI should display for `id`: the staged edit if present, else the
    /// original. `None` if the setting has not been loaded.
    pub fn value(&self, id: SettingId) -> Option<&Value> {
        self.settings.get(&id).map(Entry::effective)
    }

    /// The on-disk baseline value for `id` (ignoring any staged edit). `None` if the
    /// setting has not been loaded. Primarily for the Apply pipeline and tests.
    pub fn original(&self, id: SettingId) -> Option<&Value> {
        self.settings.get(&id).map(|entry| &entry.original)
    }

    /// The freshness tracker holding the baselines recorded when the backing files
    /// were loaded, for the Apply pipeline's conflict check (task 4.5/5.4; R5.6).
    ///
    /// The Apply pipeline ([`crate::core::apply::run`]) is a pure orchestrator that
    /// takes the tracker by reference so its step-2 conflict check measures the target
    /// files against the *store's* real baselines — the exact bytes it parsed each
    /// original from — rather than an empty tracker that could never detect an
    /// external edit. Exposed as a shared borrow because `run` only reads it;
    /// re-baselining after a successful apply happens through
    /// [`commit_apply`](Self::commit_apply), which the caller invokes with `&mut self`
    /// once the borrow returned here has ended.
    pub fn freshness(&self) -> &FreshnessTracker {
        &self.freshness
    }

    /// Whether `path` is a backing file this store already tracks, i.e. one that was
    /// loaded through [`load_file`](Self::load_file).
    ///
    /// The manual-refresh flow (R4.3) uses this to split the files a fresh load pass
    /// found into two classes: a tracked file's external changes go through
    /// [`refresh`](Self::refresh) (reload with staged edits preserved, plus a conflict
    /// report the UI can warn from, R5.6), while a file that appeared since the last
    /// load has no baseline to conflict with and is simply loaded fresh.
    pub fn is_tracked(&self, path: &Path) -> bool {
        self.reloaders.contains_key(path)
    }

    /// Re-reads the tracked backing files and reloads the originals of any that were
    /// edited externally since the store last read them (R5.6).
    ///
    /// This is called on window focus / manual refresh. For each file whose bytes no
    /// longer match the recorded baseline, the store re-reads it through its
    /// [`FileReader`], replaces the affected originals, and re-baselines the freshness
    /// tracker from the new bytes — so a subsequent Apply measures dirtiness and
    /// conflicts against the file's current contents rather than clobbering the
    /// external edit. Pending staged edits are preserved; the returned
    /// [`RefreshReport`] tells the UI which files changed so it can warn the user. A
    /// file that changed but can no longer be read or parsed is reported as failed and
    /// keeps its last known originals; one bad file never aborts the others.
    pub fn refresh(&mut self) -> RefreshReport {
        let mut report = RefreshReport::default();

        // `check_conflicts` returns owned conflicts, so the freshness borrow ends
        // before the loop and the per-file reloads can borrow the store mutably.
        for conflict in self.freshness.check_conflicts() {
            let path = conflict.path().to_path_buf();
            match conflict.reason() {
                ConflictReason::ContentChanged => match self.reload_file(&path) {
                    Ok(()) => {
                        tracing::info!(
                            path = %path.display(),
                            "reloaded originals of an externally-changed file"
                        );
                        report.reloaded.push(path);
                    }
                    Err(error) => {
                        tracing::error!(
                            path = %path.display(),
                            %error,
                            "external change detected but the file could not be reloaded; \
                             keeping last-known originals"
                        );
                        report.failed.push(path);
                    }
                },
                ConflictReason::Unreadable(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "tracked file is no longer readable; cannot reload its originals"
                    );
                    report.failed.push(path);
                }
            }
        }

        report
    }

    /// Re-reads `path` through its registered [`FileReader`] and folds the fresh
    /// values back into the store's originals, re-baselining freshness.
    ///
    /// Returns an error if no reader is registered for the path (a programming error)
    /// or if the reader itself fails to read/parse the file.
    fn reload_file(&mut self, path: &Path) -> io::Result<()> {
        let reader = self.reloaders.get(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no reader registered for {}", path.display()),
            )
        })?;
        // The owned `FileValues` ends the immutable borrow of `self.reloaders`, so the
        // mutable ingest below is allowed.
        let values = reader(path)?;
        self.ingest(path, values);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A reader for a one-line file holding a single integer, mapped to
    /// [`SettingId::DimTimeout`]. Mirrors what a real parser-backed loader
    /// would provide, but small enough to keep the store tests focused on staging.
    fn timeout_reader() -> FileReader {
        Box::new(|path: &Path| {
            let bytes = fs::read(path)?;
            let text = String::from_utf8_lossy(&bytes);
            let seconds: i64 = text.trim().parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "not an integer timeout")
            })?;
            Ok(FileValues {
                bytes,
                values: vec![(SettingId::DimTimeout, Value::Integer(seconds))],
            })
        })
    }

    /// A reader for a one-line file holding a single float, mapped to
    /// [`SettingId::MonitorScale`].
    fn scale_reader() -> FileReader {
        Box::new(|path: &Path| {
            let bytes = fs::read(path)?;
            let text = String::from_utf8_lossy(&bytes);
            let scale: f64 = text
                .trim()
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not a scale"))?;
            Ok(FileValues {
                bytes,
                values: vec![(SettingId::MonitorScale, Value::Float(scale))],
            })
        })
    }

    /// Loads a `DimTimeout` setting into `store` from a freshly written file,
    /// returning the file path so a test can edit it externally afterwards.
    fn load_timeout(store: &mut SettingsStore, dir: &Path, seconds: i64) -> PathBuf {
        let path = dir.join("timeout.conf");
        fs::write(&path, seconds.to_string()).expect("write the timeout fixture");
        let bytes = fs::read(&path).expect("read back the timeout fixture");
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: vec![(SettingId::DimTimeout, Value::Integer(seconds))],
            },
            timeout_reader(),
        );
        path
    }

    #[test]
    fn stage_marks_dirty_then_reset_returns_clean() {
        // Accept criterion: stage -> dirty -> reset -> clean, with the effective value
        // tracking staged then reverting to the original.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        load_timeout(&mut store, dir.path(), 300);

        assert!(!store.is_dirty(), "a freshly loaded store is clean");
        assert_eq!(
            store.value(SettingId::DimTimeout),
            Some(&Value::Integer(300))
        );

        let outcome = store
            .stage(SettingId::DimTimeout, Value::Integer(120))
            .expect("a valid file-backed edit stages");
        assert_eq!(outcome, StageOutcome::Staged);
        assert!(
            store.is_dirty(),
            "staging a different value makes the store dirty"
        );
        assert!(store.is_category_dirty(Category::PowerAndIdle));
        assert_eq!(
            store.value(SettingId::DimTimeout),
            Some(&Value::Integer(120))
        );
        assert_eq!(store.dirty_ids(), vec![SettingId::DimTimeout]);

        store.reset();
        assert!(!store.is_dirty(), "reset discards staged edits");
        assert!(store.dirty_ids().is_empty());
        assert_eq!(
            store.value(SettingId::DimTimeout),
            Some(&Value::Integer(300)),
            "after reset the effective value is the original again"
        );
    }

    #[test]
    fn commit_apply_promotes_staged_and_rebaselines_written_files() {
        // The task-4.5 commit contract: after a successful apply the store must
        // become clean with the staged values as the new originals, AND treat the
        // just-written on-disk bytes as the freshness baseline — so the app's own
        // write is not a spurious conflict on the next refresh/apply.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        let path = load_timeout(&mut store, dir.path(), 300);

        store
            .stage(SettingId::DimTimeout, Value::Integer(120))
            .expect("stage a pending edit");
        assert!(store.is_dirty());

        // The Apply pipeline would have written the new value to disk; mimic that,
        // then commit with the exact bytes written (as the pipeline reports them).
        fs::write(&path, "120").expect("apply writes the new value to disk");
        store.commit_apply(&[(path.clone(), b"120".to_vec())]);

        assert!(!store.is_dirty(), "commit clears dirty");
        assert!(store.dirty_ids().is_empty());
        assert_eq!(
            store.original(SettingId::DimTimeout),
            Some(&Value::Integer(120)),
            "the staged value is promoted to the original"
        );

        // The re-baseline is load-bearing: a refresh now finds no conflict even
        // though the file differs from the pre-apply baseline (300). Without the
        // re-baseline this would report the app's own write as an external change.
        assert!(
            store.refresh().is_empty(),
            "the written bytes are the new baseline, so refresh finds no self-conflict"
        );
    }

    #[test]
    fn staging_the_original_value_is_not_dirty() {
        // A staged value equal to the original must not read as dirty, so re-selecting
        // the current value never lights up Apply.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        load_timeout(&mut store, dir.path(), 300);

        let outcome = store
            .stage(SettingId::DimTimeout, Value::Integer(300))
            .expect("staging the original value succeeds");
        assert_eq!(outcome, StageOutcome::Staged);
        assert!(
            !store.is_dirty(),
            "staging a value equal to the original is not a change"
        );
    }

    #[test]
    fn dirty_rolls_up_per_page() {
        // Accept criterion (per-page rollup): dirtying one page's setting flags only
        // that page, and the global flag; dirtying the other page flags it too.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        load_timeout(&mut store, dir.path(), 300);

        // A second file-backed setting on a different page (Display).
        let scale_path = dir.path().join("scale.conf");
        fs::write(&scale_path, "1.0").expect("write the scale fixture");
        let scale_bytes = fs::read(&scale_path).expect("read back the scale fixture");
        store.load_file(
            &scale_path,
            FileValues {
                bytes: scale_bytes,
                values: vec![(SettingId::MonitorScale, Value::Float(1.0))],
            },
            scale_reader(),
        );

        store
            .stage(SettingId::DimTimeout, Value::Integer(120))
            .expect("stage the dim timeout");
        assert!(store.is_category_dirty(Category::PowerAndIdle));
        assert!(
            !store.is_category_dirty(Category::Display),
            "editing Power & Idle must not flag Display"
        );

        store
            .stage(SettingId::MonitorScale, Value::Float(1.5))
            .expect("stage the monitor scale");
        assert!(store.is_category_dirty(Category::Display));
        assert_eq!(
            store.dirty_ids(),
            vec![SettingId::MonitorScale, SettingId::DimTimeout],
            "both settings are dirty, reported in SettingId order"
        );
    }

    #[test]
    fn dirty_in_category_returns_only_that_pages_dirty_settings_with_values() {
        // The reusable Apply-glue seam (task 6.6/6.7): only the dirty settings of the
        // requested page are returned, each with its effective (staged) value, and a
        // clean page yields nothing.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        load_timeout(&mut store, dir.path(), 300);

        // A second file-backed setting on a different page (Display).
        let scale_path = dir.path().join("scale.conf");
        fs::write(&scale_path, "1.0").expect("write the scale fixture");
        let scale_bytes = fs::read(&scale_path).expect("read back the scale fixture");
        store.load_file(
            &scale_path,
            FileValues {
                bytes: scale_bytes,
                values: vec![(SettingId::MonitorScale, Value::Float(1.0))],
            },
            scale_reader(),
        );

        // Nothing staged yet: both pages are clean.
        assert!(store.dirty_in_category(Category::PowerAndIdle).is_empty());
        assert!(store.dirty_in_category(Category::Display).is_empty());

        store
            .stage(SettingId::DimTimeout, Value::Integer(120))
            .expect("stage the dim timeout");

        // Only the dirtied Power & Idle setting is returned, carrying its staged value;
        // the untouched Display page is still empty.
        assert_eq!(
            store.dirty_in_category(Category::PowerAndIdle),
            vec![(SettingId::DimTimeout, Value::Integer(120))]
        );
        assert!(
            store.dirty_in_category(Category::Display).is_empty(),
            "a clean page contributes no writes"
        );
    }

    #[test]
    fn runtime_only_setting_bypasses_staging() {
        // Accept criterion (bypass): a runtime-only edit is reported for immediate
        // application, stores nothing, and never shows as dirty (R5.2).
        let mut store = SettingsStore::new();

        let outcome = store
            .stage(SettingId::LaptopDisplayEnabled, Value::Bool(true))
            .expect("a runtime-only edit is accepted");
        assert_eq!(outcome, StageOutcome::RuntimeBypass);
        assert!(
            !store.is_dirty(),
            "a runtime-only edit never makes the store dirty"
        );
        assert!(store.dirty_ids().is_empty());
        assert!(
            store.value(SettingId::LaptopDisplayEnabled).is_none(),
            "the store holds no value for a runtime-only setting"
        );
        assert!(
            !store.is_category_dirty(Category::Display),
            "a runtime-only edit on the Display page never flags the page"
        );
    }

    #[test]
    fn staging_nan_is_rejected_and_leaves_state_well_behaved() {
        // Dirty-robustness: a NaN float is rejected on stage, so it
        // never enters `staged`. An earlier valid edit is untouched by the rejection,
        // and reset still returns the store to clean.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        let scale_path = dir.path().join("scale.conf");
        fs::write(&scale_path, "1.0").expect("write the scale fixture");
        let scale_bytes = fs::read(&scale_path).expect("read back the scale fixture");
        store.load_file(
            &scale_path,
            FileValues {
                bytes: scale_bytes,
                values: vec![(SettingId::MonitorScale, Value::Float(1.0))],
            },
            scale_reader(),
        );

        // A valid in-range edit stages and is dirty.
        store
            .stage(SettingId::MonitorScale, Value::Float(1.5))
            .expect("a valid scale stages");
        assert!(store.is_dirty());

        // A NaN is rejected as invalid; nothing about the staged state changes.
        let error = store
            .stage(SettingId::MonitorScale, Value::Float(f64::NAN))
            .expect_err("a NaN scale must be rejected");
        assert!(matches!(error, StageError::Invalid(_)));
        assert_eq!(
            store.value(SettingId::MonitorScale),
            Some(&Value::Float(1.5)),
            "a rejected NaN must not overwrite the previously staged value"
        );

        // Reset still cleans up despite the rejected NaN — the store never went into
        // a permanently-dirty state.
        store.reset();
        assert!(!store.is_dirty(), "reset returns the store to clean");
        assert_eq!(
            store.value(SettingId::MonitorScale),
            Some(&Value::Float(1.0))
        );
    }

    #[test]
    fn staging_an_out_of_range_value_is_rejected() {
        // Validate-on-stage more generally (R8.3): an out-of-range value is refused
        // and never staged.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        let scale_path = dir.path().join("scale.conf");
        fs::write(&scale_path, "1.0").expect("write the scale fixture");
        let scale_bytes = fs::read(&scale_path).expect("read back the scale fixture");
        store.load_file(
            &scale_path,
            FileValues {
                bytes: scale_bytes,
                values: vec![(SettingId::MonitorScale, Value::Float(1.0))],
            },
            scale_reader(),
        );

        let error = store
            .stage(SettingId::MonitorScale, Value::Float(10.0))
            .expect_err("a scale outside the range must be rejected");
        assert!(matches!(error, StageError::Invalid(_)));
        assert!(!store.is_dirty(), "a rejected value must not be staged");
    }

    #[test]
    fn staging_an_empty_keyboard_layout_list_is_rejected() {
        // R8.3: removing every layout stages `KeyboardLayouts = ""`,
        // which must be refused on stage — so the framework's invalid-edit path reverts
        // the control rather than letting a bare `kb_layout=` reach Apply.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = dir.path().join("input.lua");
        fs::write(&path, "us,se").expect("write the layout fixture");
        let bytes = fs::read(&path).expect("read back the layout fixture");
        let mut store = SettingsStore::new();
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: vec![(
                    SettingId::KeyboardLayouts,
                    Value::String("us,se".to_string()),
                )],
            },
            Box::new(|path: &Path| {
                let bytes = fs::read(path)?;
                Ok(FileValues {
                    bytes,
                    values: Vec::new(),
                })
            }),
        );

        let error = store
            .stage(SettingId::KeyboardLayouts, Value::String(String::new()))
            .expect_err("an empty layout list must be rejected");
        assert!(matches!(error, StageError::Invalid(_)));
        assert!(!store.is_dirty(), "a rejected edit must not be staged");

        // A non-empty reorder still stages fine.
        assert_eq!(
            store
                .stage(
                    SettingId::KeyboardLayouts,
                    Value::String("se,us".to_string())
                )
                .expect("a non-empty layout list stages"),
            StageOutcome::Staged
        );
    }

    #[test]
    fn staging_an_unsafe_lock_command_is_rejected_and_reverts() {
        // R8.3: the hypridle lock command is free text typed into an
        // Entry, but is written into a hyprlang value — a `#` would be read as a comment
        // and truncate it. Staging such a value must be refused so the framework's
        // invalid-edit path reverts the control on the offending keystroke, rather than the
        // value staging cleanly and only failing the whole Apply.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = dir.path().join("hypridle.conf");
        fs::write(&path, "general { lock_cmd = hyprlock }").expect("write the lock fixture");
        let bytes = fs::read(&path).expect("read back the lock fixture");
        let mut store = SettingsStore::new();
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: vec![(
                    SettingId::LockCommand,
                    Value::String("hyprlock".to_string()),
                )],
            },
            Box::new(|path: &Path| {
                let bytes = fs::read(path)?;
                Ok(FileValues {
                    bytes,
                    values: Vec::new(),
                })
            }),
        );

        let error = store
            .stage(
                SettingId::LockCommand,
                Value::String("hyprlock # oops".to_string()),
            )
            .expect_err("a lock command containing `#` must be rejected");
        assert!(matches!(error, StageError::Invalid(_)));
        assert!(!store.is_dirty(), "a rejected edit must not be staged");
        assert_eq!(
            store.value(SettingId::LockCommand),
            Some(&Value::String("hyprlock".to_string())),
            "the store reverts to the original, unchanged value"
        );

        // A safe lock command still stages fine.
        assert_eq!(
            store
                .stage(
                    SettingId::LockCommand,
                    Value::String("hyprlock --immediate".to_string())
                )
                .expect("a safe lock command stages"),
            StageOutcome::Staged
        );
    }

    #[test]
    fn staging_an_unloaded_file_backed_setting_is_an_error() {
        // Staging a file-backed setting the loader never registered is a guarded
        // error (NotLoaded), not a silent insert.
        let mut store = SettingsStore::new();
        let error = store
            .stage(SettingId::DimTimeout, Value::Integer(120))
            .expect_err("an unloaded setting cannot be staged");
        assert!(matches!(
            error,
            StageError::NotLoaded(SettingId::DimTimeout)
        ));
    }

    #[test]
    fn external_change_triggers_reload_of_originals() {
        // Accept criterion (conflict reload, R5.6): an external edit between load and
        // refresh is detected and the affected original is reloaded from disk; a
        // further refresh is quiet because the baseline was re-recorded.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        let path = load_timeout(&mut store, dir.path(), 300);

        // No external change yet: refresh is quiet.
        assert!(
            store.refresh().is_empty(),
            "an untouched file is not a conflict"
        );

        // Someone edits the file by hand.
        fs::write(&path, "600").expect("external edit");

        let report = store.refresh();
        assert_eq!(
            report.reloaded(),
            std::slice::from_ref(&path),
            "the edited file is reloaded"
        );
        assert!(report.failed().is_empty());
        assert_eq!(
            store.original(SettingId::DimTimeout),
            Some(&Value::Integer(600)),
            "the original is refreshed from the new file contents"
        );

        // The baseline was re-recorded, so a subsequent refresh sees no conflict.
        assert!(
            store.refresh().is_empty(),
            "re-baselining must clear the conflict"
        );
    }

    #[test]
    fn reload_preserves_a_pending_staged_edit() {
        // On conflict the store reloads the original but keeps the user's pending edit
        // (no silent clobber, R5.6): the setting stays dirty, now measured against the
        // new baseline, and the effective value is still the staged one.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        let path = load_timeout(&mut store, dir.path(), 300);

        store
            .stage(SettingId::DimTimeout, Value::Integer(120))
            .expect("stage a pending edit");

        fs::write(&path, "600").expect("external edit");
        let report = store.refresh();
        assert_eq!(report.reloaded(), &[path]);

        assert_eq!(
            store.original(SettingId::DimTimeout),
            Some(&Value::Integer(600)),
            "original refreshed to the external value"
        );
        assert_eq!(
            store.value(SettingId::DimTimeout),
            Some(&Value::Integer(120)),
            "the pending staged edit is preserved"
        );
        assert!(
            store.is_dirty(),
            "the preserved edit still differs from the new original"
        );
    }

    #[test]
    fn a_deleted_backing_file_is_reported_as_failed() {
        // A file that vanishes between load and refresh cannot be reloaded, so it is
        // reported as failed and its last-known original is retained.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        let path = load_timeout(&mut store, dir.path(), 300);

        fs::remove_file(&path).expect("delete the tracked file");

        let report = store.refresh();
        assert!(report.reloaded().is_empty());
        assert_eq!(
            report.failed(),
            &[path],
            "a deleted file is reported as failed"
        );
        assert_eq!(
            store.original(SettingId::DimTimeout),
            Some(&Value::Integer(300)),
            "the last-known original is retained when reload fails"
        );
    }

    #[test]
    fn reset_after_a_conflict_reload_lands_on_the_new_baseline() {
        // The load-bearing consequence of "preserve staged over a conflict, but
        // re-baseline the original": once the user discards their pending edit with
        // reset, the effective value must be the reloaded (new) original, never the
        // stale pre-conflict one. Staging 120 over a loaded 300, then an external edit
        // to 600, then reset, must settle on 600 and be clean.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let mut store = SettingsStore::new();
        let path = load_timeout(&mut store, dir.path(), 300);

        store
            .stage(SettingId::DimTimeout, Value::Integer(120))
            .expect("stage a pending edit");

        fs::write(&path, "600").expect("external edit");
        assert_eq!(store.refresh().reloaded(), &[path]);

        store.reset();
        assert!(!store.is_dirty(), "reset clears the pending edit");
        assert_eq!(
            store.value(SettingId::DimTimeout),
            Some(&Value::Integer(600)),
            "after reset the effective value is the re-baselined original, not the \
             stale pre-conflict 300"
        );
    }

    #[test]
    fn load_file_skips_a_runtime_only_setting() {
        // Invariant: the store holds only file-backed settings. A runtime-only id
        // offered in a file's values is ignored by `ingest`, never stored — so it has
        // no effective value and cannot become dirty (R5.2).
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = dir.path().join("mixed.conf");
        fs::write(&path, "unused").expect("write the fixture");
        let bytes = fs::read(&path).expect("read back the fixture");

        let mut store = SettingsStore::new();
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: vec![
                    (SettingId::DimTimeout, Value::Integer(300)),
                    (SettingId::LaptopDisplayEnabled, Value::Bool(true)),
                ],
            },
            timeout_reader(),
        );

        assert_eq!(
            store.value(SettingId::DimTimeout),
            Some(&Value::Integer(300)),
            "the file-backed setting is stored"
        );
        assert!(
            store.value(SettingId::LaptopDisplayEnabled).is_none(),
            "the runtime-only setting is skipped, never stored"
        );
    }

    // --- Linked settings: the lock command and its grace period (task 11.13) ---

    /// A store loaded with the real dotfiles' lock command and the grace period the
    /// loader derives from it (`--grace 10`), under a synthetic key that is never read.
    fn lock_store(command: &str, grace: Option<i64>) -> SettingsStore {
        let mut values = vec![(SettingId::LockCommand, Value::String(command.to_string()))];
        if let Some(seconds) = grace {
            values.push((SettingId::LockGracePeriod, Value::Integer(seconds)));
        }
        let mut store = SettingsStore::new();
        store.load_file(
            "<lock test seed>",
            FileValues {
                bytes: Vec::new(),
                values,
            },
            Box::new(|_: &Path| Err(io::Error::other("never re-read in these tests"))),
        );
        store
    }

    fn lock_command(store: &SettingsStore) -> Option<&str> {
        store.value(SettingId::LockCommand).and_then(Value::as_str)
    }

    const REAL_LOCK_CMD: &str = "pidof hyprlock || hyprlock --grace 10";

    #[test]
    fn staging_a_grace_period_rewrites_the_lock_command_too() {
        let mut store = lock_store(REAL_LOCK_CMD, Some(10));
        store
            .stage(SettingId::LockGracePeriod, Value::Integer(30))
            .expect("a grace period stages");
        assert_eq!(
            lock_command(&store),
            Some("pidof hyprlock || hyprlock --grace 30"),
            "the lock-command field must show the new token, not stale text"
        );
        assert_eq!(
            store.dirty_ids(),
            vec![SettingId::LockCommand, SettingId::LockGracePeriod],
            "both views of general.lock_cmd are dirty together"
        );

        // Back to the original: both settle back to clean, byte-identical text.
        store
            .stage(SettingId::LockGracePeriod, Value::Integer(10))
            .expect("the original grace stages");
        assert_eq!(lock_command(&store), Some(REAL_LOCK_CMD));
        assert!(!store.is_dirty());
    }

    #[test]
    fn staging_a_lock_command_updates_the_grace_period() {
        let mut store = lock_store(REAL_LOCK_CMD, Some(10));
        store
            .stage(
                SettingId::LockCommand,
                Value::String("hyprlock --grace 45 --no-fade-in".to_string()),
            )
            .expect("a lock command stages");
        assert_eq!(
            store.value(SettingId::LockGracePeriod),
            Some(&Value::Integer(45))
        );

        // No grace flag reads as zero, which hyprlock does without one.
        store
            .stage(
                SettingId::LockCommand,
                Value::String("hyprlock".to_string()),
            )
            .expect("a lock command stages");
        assert_eq!(
            store.value(SettingId::LockGracePeriod),
            Some(&Value::Integer(0))
        );

        // Commit promotes both, so the pair stays coherent after an Apply.
        store.commit_apply(&[]);
        assert_eq!(
            store.original(SettingId::LockGracePeriod),
            Some(&Value::Integer(0))
        );
        assert_eq!(
            store.original(SettingId::LockCommand),
            Some(&Value::String("hyprlock".to_string()))
        );
    }

    #[test]
    fn a_lock_command_without_hyprlock_discards_a_pending_grace_edit() {
        let mut store = lock_store(REAL_LOCK_CMD, Some(10));
        store
            .stage(SettingId::LockGracePeriod, Value::Integer(30))
            .expect("a grace period stages");
        store
            .stage(
                SettingId::LockCommand,
                Value::String("swaylock -f".to_string()),
            )
            .expect("a lock command without hyprlock still stages");
        assert_eq!(
            store.dirty_ids(),
            vec![SettingId::LockCommand],
            "the grace edit no longer fits the command and is dropped"
        );
        assert_eq!(
            store.value(SettingId::LockGracePeriod),
            Some(&Value::Integer(10))
        );

        // ...and a new grace period is now refused, leaving everything as it was.
        let error = store
            .stage(SettingId::LockGracePeriod, Value::Integer(5))
            .expect_err("there is no hyprlock to give a grace period");
        assert!(matches!(
            error,
            StageError::Invalid(ValidationError::LockGraceUnavailable(_))
        ));
        assert_eq!(lock_command(&store), Some("swaylock -f"));
        assert_eq!(store.dirty_ids(), vec![SettingId::LockCommand]);
    }

    #[test]
    fn a_lock_command_with_an_out_of_range_grace_is_refused_whole() {
        // R8.3: the grace range binds a grace typed into the free-text command too; the
        // refusal stages neither half.
        let mut store = lock_store(REAL_LOCK_CMD, Some(10));
        let error = store
            .stage(
                SettingId::LockCommand,
                Value::String("hyprlock --grace 3600".to_string()),
            )
            .expect_err("an hour of grace is beyond the range");
        assert!(matches!(
            error,
            StageError::Invalid(ValidationError::OutOfRange { .. })
        ));
        assert!(!store.is_dirty());
        assert_eq!(lock_command(&store), Some(REAL_LOCK_CMD));
    }

    #[test]
    fn a_lock_command_edit_without_a_loaded_grace_stages_alone() {
        // The loader seeds no grace period for a command that does not start hyprlock;
        // its lock-command edits then behave exactly as before task 11.13.
        let mut store = lock_store("swaylock -f", None);
        store
            .stage(
                SettingId::LockCommand,
                Value::String("hyprlock --grace 5".to_string()),
            )
            .expect("a lock command stages");
        assert_eq!(store.dirty_ids(), vec![SettingId::LockCommand]);
        assert!(store.value(SettingId::LockGracePeriod).is_none());
    }

    #[test]
    fn reset_after_a_linked_stage_clears_both_settings() {
        let mut store = lock_store(REAL_LOCK_CMD, Some(10));
        store
            .stage(SettingId::LockGracePeriod, Value::Integer(30))
            .expect("a grace period stages");
        assert_eq!(store.dirty_ids().len(), 2);

        store.reset();
        assert!(!store.is_dirty());
        assert_eq!(lock_command(&store), Some(REAL_LOCK_CMD));
        assert_eq!(
            store.value(SettingId::LockGracePeriod),
            Some(&Value::Integer(10))
        );
    }

    #[test]
    fn retyping_the_original_lock_command_leaves_the_store_clean() {
        // A pending grace edit, then the user types the lock command back to exactly its
        // on-disk text: the grace re-derived from that text is the original 10, so
        // neither view is dirty.
        let mut store = lock_store(REAL_LOCK_CMD, Some(10));
        store
            .stage(SettingId::LockGracePeriod, Value::Integer(30))
            .expect("a grace period stages");
        store
            .stage(
                SettingId::LockCommand,
                Value::String(REAL_LOCK_CMD.to_string()),
            )
            .expect("the original command stages");
        assert!(!store.is_dirty(), "dirty: {:?}", store.dirty_ids());
        assert_eq!(
            store.value(SettingId::LockGracePeriod),
            Some(&Value::Integer(10))
        );
    }

    #[test]
    fn a_refresh_while_the_pair_is_dirty_keeps_both_staged_values() {
        // R5.6: an external edit reloads the originals of both views, and the pending
        // linked edit survives as a pair, so the two still agree with each other.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = dir.path().join("lock_cmd");
        fs::write(&path, REAL_LOCK_CMD).expect("write the fixture");
        // Mirrors the hypridle.conf loader: the file is just the lock command here, and
        // the grace period is derived from it.
        let reader: FileReader = Box::new(|path: &Path| {
            let bytes = fs::read(path)?;
            let command = String::from_utf8_lossy(&bytes).into_owned();
            let mut values = vec![(SettingId::LockCommand, Value::String(command.clone()))];
            if let Ok(seconds) = parse_grace(&command) {
                values.push((
                    SettingId::LockGracePeriod,
                    Value::Integer(i64::from(seconds.unwrap_or(0))),
                ));
            }
            Ok(FileValues { bytes, values })
        });
        let initial = reader(&path).expect("the fixture loads");
        let mut store = SettingsStore::new();
        store.load_file(&path, initial, reader);

        store
            .stage(SettingId::LockGracePeriod, Value::Integer(30))
            .expect("a grace period stages");
        fs::write(&path, "hyprlock --grace 5 -q").expect("edit the file externally");

        let report = store.refresh();
        assert_eq!(report.reloaded(), std::slice::from_ref(&path));
        assert_eq!(
            store.original(SettingId::LockCommand),
            Some(&Value::String("hyprlock --grace 5 -q".to_string()))
        );
        assert_eq!(
            store.original(SettingId::LockGracePeriod),
            Some(&Value::Integer(5))
        );
        assert_eq!(
            lock_command(&store),
            Some("pidof hyprlock || hyprlock --grace 30"),
            "the staged lock command is kept"
        );
        assert_eq!(
            store.value(SettingId::LockGracePeriod),
            Some(&Value::Integer(30)),
            "the staged grace is kept"
        );
        assert_eq!(
            store.dirty_ids(),
            vec![SettingId::LockCommand, SettingId::LockGracePeriod]
        );
    }

    #[test]
    fn an_out_of_range_grace_on_disk_blocks_lock_command_edits_until_lowered() {
        // The documented corner (see `linked_edit`): a hand-written `--grace 3600` loads
        // verbatim, an edit that keeps the token is refused, and the spin button's
        // in-range value is the way out, rewriting the token in place.
        const ON_DISK: &str = "pidof hyprlock || hyprlock --grace 3600";
        let mut store = lock_store(ON_DISK, Some(3600));
        assert_eq!(
            store.value(SettingId::LockGracePeriod),
            Some(&Value::Integer(3600))
        );

        let error = store
            .stage(
                SettingId::LockCommand,
                Value::String(format!("{ON_DISK} -q")),
            )
            .expect_err("keeping the out-of-range token is refused");
        assert!(matches!(
            error,
            StageError::Invalid(ValidationError::OutOfRange { .. })
        ));
        assert!(!store.is_dirty());

        store
            .stage(SettingId::LockGracePeriod, Value::Integer(60))
            .expect("an in-range grace stages");
        assert_eq!(
            lock_command(&store),
            Some("pidof hyprlock || hyprlock --grace 60")
        );
    }
}
