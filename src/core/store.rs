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
use crate::core::model::{Backing, Category, SettingId, ValidationError, Value};

/// The freshly-read contents of one backing file: the raw bytes plus the original
/// values parsed from them.
///
/// This is what a [`FileReader`] returns and what [`SettingsStore::load_file`] is
/// given for the initial load. Bundling the bytes with the values guarantees the two
/// come from a *single* read of the file, which is what lets the store baseline the
/// freshness tracker against the exact bytes it parsed (see the module docs).
#[derive(Debug)]
pub(crate) struct FileValues {
    /// The file's exact bytes at the moment it was read, used verbatim as the
    /// freshness baseline.
    pub(crate) bytes: Vec<u8>,
    /// The original values parsed from those bytes, each keyed by the setting it
    /// backs. A runtime-only [`SettingId`] appearing here is ignored (the store only
    /// tracks file-backed settings).
    pub(crate) values: Vec<(SettingId, Value)>,
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
pub(crate) type FileReader = Box<dyn Fn(&Path) -> io::Result<FileValues> + Send>;

/// The outcome of a successful [`SettingsStore::stage`] call.
///
/// Tells the caller whether the value was recorded as a pending file-backed edit or
/// must be applied immediately because the setting is runtime-only (R5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StageOutcome {
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
pub(crate) enum StageError {
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
pub(crate) struct RefreshReport {
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
    pub(crate) fn is_empty(&self) -> bool {
        self.reloaded.is_empty() && self.failed.is_empty()
    }

    /// The files whose originals were reloaded from disk.
    pub(crate) fn reloaded(&self) -> &[PathBuf] {
        &self.reloaded
    }

    /// The files that changed but could not be reloaded.
    pub(crate) fn failed(&self) -> &[PathBuf] {
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
pub(crate) struct SettingsStore {
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

impl SettingsStore {
    /// Creates an empty store with no loaded settings.
    pub(crate) fn new() -> Self {
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
    pub(crate) fn load_file(
        &mut self,
        path: impl Into<PathBuf>,
        initial: FileValues,
        reader: FileReader,
    ) {
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
    pub(crate) fn stage(
        &mut self,
        id: SettingId,
        value: Value,
    ) -> Result<StageOutcome, StageError> {
        id.validate(&value)?;

        if id.backing() == Backing::RuntimeOnly {
            // Runtime-only (R5.2): applied immediately by the caller, never staged.
            tracing::debug!(?id, "runtime-only edit bypasses staging");
            return Ok(StageOutcome::RuntimeBypass);
        }

        match self.settings.get_mut(&id) {
            Some(entry) => {
                entry.staged = Some(value);
                tracing::debug!(?id, dirty = entry.is_dirty(), "staged edit");
                Ok(StageOutcome::Staged)
            }
            None => Err(StageError::NotLoaded(id)),
        }
    }

    /// Discards every staged edit, returning the store to a clean state (R5.1).
    pub(crate) fn reset(&mut self) {
        for entry in self.settings.values_mut() {
            entry.staged = None;
        }
        tracing::debug!("discarded all staged edits");
    }

    /// Whether any file-backed setting has a pending edit differing from its original.
    ///
    /// Drives the suggested-action Apply button (R5.1).
    pub(crate) fn is_dirty(&self) -> bool {
        self.settings.values().any(Entry::is_dirty)
    }

    /// Whether any setting on `category`'s page is dirty — the per-page rollup that
    /// drives the sidebar's modified-dot markers (R5.1).
    pub(crate) fn is_category_dirty(&self, category: Category) -> bool {
        self.settings
            .iter()
            .any(|(id, entry)| id.category() == category && entry.is_dirty())
    }

    /// The ids of all currently dirty settings, in [`SettingId`] order.
    ///
    /// Useful for the Apply pipeline (task 4.5), which writes only the changed
    /// settings, and for asserting the rollup in tests.
    pub(crate) fn dirty_ids(&self) -> Vec<SettingId> {
        self.settings
            .iter()
            .filter(|(_, entry)| entry.is_dirty())
            .map(|(id, _)| *id)
            .collect()
    }

    /// The value the UI should display for `id`: the staged edit if present, else the
    /// original. `None` if the setting has not been loaded.
    pub(crate) fn value(&self, id: SettingId) -> Option<&Value> {
        self.settings.get(&id).map(Entry::effective)
    }

    /// The on-disk baseline value for `id` (ignoring any staged edit). `None` if the
    /// setting has not been loaded. Primarily for the Apply pipeline and tests.
    pub(crate) fn original(&self, id: SettingId) -> Option<&Value> {
        self.settings.get(&id).map(|entry| &entry.original)
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
    pub(crate) fn refresh(&mut self) -> RefreshReport {
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
    /// [`SettingId::NotificationTimeout`]. Mirrors what a real parser-backed loader
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
                values: vec![(SettingId::NotificationTimeout, Value::Integer(seconds))],
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

    /// Loads a `NotificationTimeout` setting into `store` from a freshly written file,
    /// returning the file path so a test can edit it externally afterwards.
    fn load_timeout(store: &mut SettingsStore, dir: &Path, seconds: i64) -> PathBuf {
        let path = dir.join("timeout.conf");
        fs::write(&path, seconds.to_string()).expect("write the timeout fixture");
        let bytes = fs::read(&path).expect("read back the timeout fixture");
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: vec![(SettingId::NotificationTimeout, Value::Integer(seconds))],
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
            store.value(SettingId::NotificationTimeout),
            Some(&Value::Integer(300))
        );

        let outcome = store
            .stage(SettingId::NotificationTimeout, Value::Integer(120))
            .expect("a valid file-backed edit stages");
        assert_eq!(outcome, StageOutcome::Staged);
        assert!(
            store.is_dirty(),
            "staging a different value makes the store dirty"
        );
        assert!(store.is_category_dirty(Category::Notifications));
        assert_eq!(
            store.value(SettingId::NotificationTimeout),
            Some(&Value::Integer(120))
        );
        assert_eq!(store.dirty_ids(), vec![SettingId::NotificationTimeout]);

        store.reset();
        assert!(!store.is_dirty(), "reset discards staged edits");
        assert!(store.dirty_ids().is_empty());
        assert_eq!(
            store.value(SettingId::NotificationTimeout),
            Some(&Value::Integer(300)),
            "after reset the effective value is the original again"
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
            .stage(SettingId::NotificationTimeout, Value::Integer(300))
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
            .stage(SettingId::NotificationTimeout, Value::Integer(120))
            .expect("stage the notification timeout");
        assert!(store.is_category_dirty(Category::Notifications));
        assert!(
            !store.is_category_dirty(Category::Display),
            "editing Notifications must not flag Display"
        );

        store
            .stage(SettingId::MonitorScale, Value::Float(1.5))
            .expect("stage the monitor scale");
        assert!(store.is_category_dirty(Category::Display));
        assert_eq!(
            store.dirty_ids(),
            vec![SettingId::MonitorScale, SettingId::NotificationTimeout],
            "both settings are dirty, reported in SettingId order"
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
        // Dirty-robustness (the 4.1 review): a NaN float is rejected on stage, so it
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
    fn staging_an_unloaded_file_backed_setting_is_an_error() {
        // Staging a file-backed setting the loader never registered is a guarded
        // error (NotLoaded), not a silent insert.
        let mut store = SettingsStore::new();
        let error = store
            .stage(SettingId::NotificationTimeout, Value::Integer(120))
            .expect_err("an unloaded setting cannot be staged");
        assert!(matches!(
            error,
            StageError::NotLoaded(SettingId::NotificationTimeout)
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
            store.original(SettingId::NotificationTimeout),
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
            .stage(SettingId::NotificationTimeout, Value::Integer(120))
            .expect("stage a pending edit");

        fs::write(&path, "600").expect("external edit");
        let report = store.refresh();
        assert_eq!(report.reloaded(), &[path]);

        assert_eq!(
            store.original(SettingId::NotificationTimeout),
            Some(&Value::Integer(600)),
            "original refreshed to the external value"
        );
        assert_eq!(
            store.value(SettingId::NotificationTimeout),
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
            store.original(SettingId::NotificationTimeout),
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
            .stage(SettingId::NotificationTimeout, Value::Integer(120))
            .expect("stage a pending edit");

        fs::write(&path, "600").expect("external edit");
        assert_eq!(store.refresh().reloaded(), &[path]);

        store.reset();
        assert!(!store.is_dirty(), "reset clears the pending edit");
        assert_eq!(
            store.value(SettingId::NotificationTimeout),
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
                    (SettingId::NotificationTimeout, Value::Integer(300)),
                    (SettingId::LaptopDisplayEnabled, Value::Bool(true)),
                ],
            },
            timeout_reader(),
        );

        assert_eq!(
            store.value(SettingId::NotificationTimeout),
            Some(&Value::Integer(300)),
            "the file-backed setting is stored"
        );
        assert!(
            store.value(SettingId::LaptopDisplayEnabled).is_none(),
            "the runtime-only setting is skipped, never stored"
        );
    }
}
