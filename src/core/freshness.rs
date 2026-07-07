//! Per-file freshness tracking for external-edit conflict detection (task 2.3;
//! architecture §3 "Write safety" and §6 step 2; R5.6).
//!
//! The app stages edits in memory and only writes them out on Apply. Between the
//! moment it reads a config file and the moment it writes the file back, the user
//! (or another tool) may have edited that same file by hand. Blindly rewriting it
//! would silently clobber those external edits. To prevent that, this module
//! records a lightweight fingerprint of each file when it is read and re-checks
//! the files before a write, reporting any that changed underneath us (R5.6).
//!
//! # Where this fits
//!
//! [`FreshnessTracker`] is domain logic, kept here in `core/` (GTK-free,
//! headlessly testable — R6.2) rather than in the `system/` side-effect layer: it
//! is the state the [`SettingsStore`](crate::core) records at read time (task 4.2)
//! and the input to the Apply pipeline's conflict-check step (task 4.5,
//! architecture §6 step 2). It reads files directly through [`std::fs`], the same
//! way `core/detect.rs` reads configs to check readability — reading is not a
//! process side effect, so it needs no `CommandRunner`; only *writes* go through
//! [`crate::system::writer`] and *commands* through
//! [`crate::system::command`].
//!
//! # The conflict rule
//!
//! A recorded file is considered **in conflict** when either:
//!
//! - its current contents no longer hash to the value captured at read time
//!   ([`ConflictReason::ContentChanged`]), or
//! - it can no longer be re-read at all — deleted, moved, or its permissions
//!   revoked ([`ConflictReason::Unreadable`]).
//!
//! The **content hash is authoritative**; the recorded modification time (mtime)
//! never independently decides the outcome. This is deliberate and is exactly
//! what the two failure modes of an mtime-only check would get wrong:
//!
//! - An editor that rewrites a file *without advancing its mtime* (or a tool that
//!   restores an old mtime) would slip past an mtime check, yet the content did
//!   change — so a content change under an unchanged mtime **must still be
//!   caught**. Because the hash is compared unconditionally on every check, it is.
//! - A pure `touch` (or an atomic rewrite to byte-identical contents) advances the
//!   mtime while leaving the bytes unchanged — that is **not** a real edit and
//!   must not raise a false conflict. Because identical bytes hash equally, it does
//!   not.
//!
//! The mtime is still captured alongside the hash — the task's record is "content
//! hash + mtime" — and is emitted in the `debug` diagnostics for a detected
//! conflict, but it is never used to short-circuit or override the hash
//! comparison.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The fingerprint of a single file captured at read time.
///
/// Holds just enough to notice a later change without keeping the file's contents
/// in memory: a content hash (the authority for the conflict decision) and the
/// last-modified time (recorded per the task's "content hash + mtime" contract and
/// used only for diagnostics — see the module docs on the conflict rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileFreshness {
    /// A non-cryptographic hash of the file's exact bytes at record time.
    ///
    /// The full contents are intentionally *not* stored — a hash is enough to
    /// detect that the bytes changed, and keeps the tracker's memory proportional
    /// to the number of files rather than their size.
    content_hash: u64,
    /// The file's last-modified time at record time, or `None` when the platform
    /// or filesystem cannot report one.
    ///
    /// This is best-effort and diagnostic only: a missing mtime never affects the
    /// conflict decision, which rests entirely on [`Self::content_hash`].
    modified: Option<SystemTime>,
}

/// Reason a tracked file is reported as a conflict by
/// [`FreshnessTracker::check_conflicts`].
///
/// Distinguishing the two lets the Apply pipeline (task 4.5) phrase an accurate
/// warning — "changed on disk" versus "no longer readable" — and decide how to
/// recover (reload the new contents, or drop the file from the plan).
#[derive(Debug)]
pub(crate) enum ConflictReason {
    /// The file is still readable but its contents differ from what was recorded,
    /// i.e. it was edited externally since the app read it.
    ContentChanged,
    /// The file could not be re-read to compare it — most often because it was
    /// deleted or moved, or its permissions were revoked. It is reported as a
    /// conflict (rather than ignored) so the pipeline never proceeds to write over
    /// a target it can no longer verify. Carries the underlying OS error.
    Unreadable(io::Error),
}

/// A tracked file whose on-disk state no longer matches what was recorded when it
/// was read.
///
/// Returned by [`FreshnessTracker::check_conflicts`]. Carries the *live* path that
/// was tracked (the same path passed to [`FreshnessTracker::record`], e.g. the XDG
/// runtime path) so the caller can surface it to the user and reload it, plus the
/// [`ConflictReason`].
#[derive(Debug)]
pub(crate) struct Conflict {
    /// The path that was tracked and is now in conflict.
    path: PathBuf,
    /// Why it is considered a conflict.
    reason: ConflictReason,
}

impl Conflict {
    /// The tracked path this conflict concerns.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Why the file is in conflict.
    pub(crate) fn reason(&self) -> &ConflictReason {
        &self.reason
    }
}

/// Records a fingerprint of each file it is told to track and re-checks them on
/// demand, reporting the files that changed since they were recorded (R5.6).
///
/// Typical lifecycle, driven by the store (task 4.2) and Apply pipeline (task
/// 4.5): [`record`](Self::record) each backing file as it is first read; call
/// [`check_conflicts`](Self::check_conflicts) before writing (and on window
/// focus/manual refresh) to catch external edits; after handling a conflict by
/// reloading a file, [`record`](Self::record) it again to re-baseline.
#[derive(Debug, Default)]
pub(crate) struct FreshnessTracker {
    /// The recorded fingerprint per tracked path.
    ///
    /// A [`BTreeMap`] rather than a `HashMap` so that [`Self::check_conflicts`]
    /// reports conflicts in a stable, path-sorted order — deterministic logs and
    /// tests, at no meaningful cost for the handful of config files involved.
    records: BTreeMap<PathBuf, FileFreshness>,
}

impl FreshnessTracker {
    /// Creates an empty tracker.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reads `path` and records its content hash and mtime as the baseline to
    /// compare against later.
    ///
    /// Reading follows symlinks, so tracking the live XDG path fingerprints the
    /// contents the desktop actually loads (R8.5) — an external edit to the real
    /// file behind a symlink is caught just the same. Re-recording an
    /// already-tracked path overwrites its baseline, which is how the caller
    /// re-baselines a file after reloading it in response to a conflict.
    ///
    /// Returns the underlying [`io::Error`] if the file cannot be read; the caller
    /// records a file straight after successfully reading it, so this is expected
    /// to succeed on the normal path, but it is fallible rather than panicking so a
    /// racing deletion is handled cleanly.
    pub(crate) fn record(&mut self, path: impl Into<PathBuf>) -> io::Result<()> {
        let path = path.into();
        let freshness = capture(&path)?;
        tracing::debug!(
            path = %path.display(),
            "recorded file freshness baseline"
        );
        self.records.insert(path, freshness);
        Ok(())
    }

    /// Records a freshness baseline for `path` from bytes the caller has *already*
    /// read, without reading the file a second time.
    ///
    /// This is the entry point the [`SettingsStore`](crate::core) uses at read time
    /// (task 4.2): the store has just read the file to parse its `original`, so it
    /// hands those exact bytes here instead of making the tracker re-read the file.
    ///
    /// Beyond saving a redundant read, this closes a narrow
    /// time-of-check/time-of-use gap that [`record`](Self::record) cannot. If the
    /// tracker read the file itself and an external edit landed *between* the
    /// store's read and the tracker's read, [`record`](Self::record) would baseline
    /// the already-edited bytes while the store's in-memory `original` still held
    /// the pre-edit bytes. A later [`check_conflicts`](Self::check_conflicts) would
    /// then see the on-disk bytes matching the (edited) baseline and report no
    /// conflict, letting a write clobber the external edit — the exact case this
    /// module exists to prevent. Fingerprinting the same bytes the store parsed
    /// keeps the baseline and the store's `original` in lockstep, so that race
    /// surfaces as a conflict instead.
    ///
    /// The content hash is computed from `contents`, which is authoritative. The
    /// mtime is read separately from the file's metadata and is best-effort: it may
    /// not correspond to exactly the same instant as `contents` (a concurrent
    /// writer could move it), and it degrades to `None` if it cannot be read. Both
    /// are harmless because the mtime is diagnostic only and never gates the
    /// conflict decision (see the module docs). Following symlinks, the mtime is
    /// taken from the real file the live path resolves to (R8.5).
    ///
    /// Re-recording an already-tracked path overwrites its baseline, exactly as
    /// [`record`](Self::record) does. Unlike [`record`](Self::record) this cannot
    /// fail: the caller already holds the bytes, and a missing mtime is tolerated.
    pub(crate) fn record_bytes(&mut self, path: impl Into<PathBuf>, contents: &[u8]) {
        let path = path.into();
        let freshness = FileFreshness {
            content_hash: hash_contents(contents),
            modified: mtime_of(&path),
        };
        tracing::debug!(
            path = %path.display(),
            "recorded file freshness baseline from caller-supplied bytes"
        );
        self.records.insert(path, freshness);
    }

    /// Whether no files are currently tracked.
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Re-reads every tracked file and returns the ones that changed since they
    /// were recorded (R5.6).
    ///
    /// An empty result means every tracked file still matches its recorded
    /// fingerprint and it is safe to proceed with a write. A non-empty result lists
    /// each changed or now-unreadable file (see [`ConflictReason`]), in
    /// path-sorted order. Every conflict is logged at `warn`; file *contents* are
    /// never logged (R7.3). This never returns an `Err`: an unreadable file is
    /// itself reported as a [`ConflictReason::Unreadable`] conflict rather than
    /// aborting the whole check, so one bad file cannot mask changes to the others.
    pub(crate) fn check_conflicts(&self) -> Vec<Conflict> {
        let mut conflicts = Vec::new();

        for (path, recorded) in &self.records {
            match capture(path) {
                Ok(current) => {
                    // The content hash is authoritative: a hash match means the
                    // bytes are unchanged and there is no conflict, even if the
                    // mtime moved (a `touch` or a rewrite to identical bytes). A
                    // hash mismatch is a real external edit regardless of what the
                    // mtime did. See the module docs for why mtime never gates this.
                    if current.content_hash != recorded.content_hash {
                        tracing::warn!(
                            path = %path.display(),
                            "tracked file changed on disk since it was read; \
                             external edits would be clobbered by a write"
                        );
                        tracing::debug!(
                            path = %path.display(),
                            recorded_modified = ?recorded.modified,
                            current_modified = ?current.modified,
                            "freshness content-hash mismatch"
                        );
                        conflicts.push(Conflict {
                            path: path.clone(),
                            reason: ConflictReason::ContentChanged,
                        });
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "tracked file could not be re-read to check for conflicts; \
                         treating it as changed"
                    );
                    conflicts.push(Conflict {
                        path: path.clone(),
                        reason: ConflictReason::Unreadable(error),
                    });
                }
            }
        }

        conflicts
    }
}

/// Reads `path` and computes its freshness fingerprint (content hash + mtime).
///
/// Shared by [`FreshnessTracker::record`] and [`FreshnessTracker::check_conflicts`]
/// so the record and the re-check always fingerprint a file the same way — the
/// only thing that guarantees a byte-identical file compares equal. Opens the file
/// once and takes the mtime from the same open handle. That gives a consistent
/// handle, not a point-in-time snapshot: the mtime and the bytes are not captured
/// atomically, so a concurrent writer between the `metadata` call and the read
/// could pair an older mtime with newer bytes. This is harmless because the mtime
/// is diagnostic only and never gates the conflict decision (see the module docs);
/// the content hash, which does, always reflects the exact bytes read here.
fn capture(path: &Path) -> io::Result<FileFreshness> {
    let mut file = File::open(path)?;

    // mtime is best-effort and diagnostic only (see `FileFreshness::modified`): a
    // filesystem that cannot report it must not break conflict detection, which
    // rests on the content hash. So a metadata/mtime failure degrades to `None`
    // rather than propagating.
    let modified = file.metadata().and_then(|meta| meta.modified()).ok();

    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;

    Ok(FileFreshness {
        content_hash: hash_contents(&contents),
        modified,
    })
}

/// Best-effort last-modified time of `path`, following symlinks.
///
/// Used by [`FreshnessTracker::record_bytes`], which fingerprints caller-supplied
/// bytes and so does not open the file to read it, but still wants the diagnostic
/// mtime. Returns `None` when the mtime cannot be read — an unsupported filesystem,
/// or the file having just vanished — because the mtime is diagnostic only and a
/// missing one must never affect a conflict decision (see [`FileFreshness::modified`]).
fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// Hashes a file's bytes into the fingerprint used for conflict detection.
///
/// Uses the standard library's [`DefaultHasher`] (SipHash-1-3 with fixed keys), so
/// identical bytes always produce the same value and no extra hashing dependency is
/// pulled in. A non-cryptographic hash is the right tool here: the goal is to
/// notice an *accidental* external edit, not to resist an adversary deliberately
/// crafting a collision, so SipHash's collision resistance is far more than enough
/// and the vanishing chance of two genuinely different config files colliding is
/// acceptable.
fn hash_contents(contents: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::fs::OpenOptions;
    use std::time::Duration;

    /// Writes `contents` to a fresh file inside `dir` and returns its path.
    fn write_file(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("test file should be writable");
        path
    }

    /// Forces `path`'s mtime to `time`, so tests can decouple the modification
    /// time from the contents. Uses [`File::set_modified`] (stable since Rust
    /// 1.75) — no sleeping and no extra dependency.
    fn set_mtime(path: &Path, time: SystemTime) {
        OpenOptions::new()
            .write(true)
            .open(path)
            .expect("test file should be openable for writing")
            .set_modified(time)
            .expect("setting the modification time should succeed");
    }

    #[test]
    fn record_captures_a_content_hash_and_mtime() {
        // The task's record is "content hash + mtime"; confirm both are captured.
        // The mtime is expected to be present on the Linux target the app runs on.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(dir.path(), "config.conf", b"key = value\n");

        let mut tracker = FreshnessTracker::new();
        assert!(tracker.is_empty());
        tracker
            .record(&path)
            .expect("recording a readable file should succeed");
        assert!(!tracker.is_empty());

        let record = tracker
            .records
            .get(&path)
            .expect("the file should now be tracked");
        assert_eq!(record.content_hash, hash_contents(b"key = value\n"));
        assert!(
            record.modified.is_some(),
            "an mtime should be captured on this platform"
        );
    }

    #[test]
    fn external_modification_between_record_and_check_is_detected() {
        // Accept criterion (primary): an external edit made after recording is
        // reported as a ContentChanged conflict on the exact tracked path.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(dir.path(), "hyprland.conf", b"env = XCURSOR_SIZE,16\n");

        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("record baseline");

        // Someone edits the file by hand between the read and the check.
        fs::write(&path, b"env = XCURSOR_SIZE,24\n").expect("external edit");

        let conflicts = tracker.check_conflicts();
        assert_eq!(conflicts.len(), 1, "the edited file must be reported");
        assert_eq!(conflicts[0].path(), path.as_path());
        assert!(
            matches!(conflicts[0].reason(), ConflictReason::ContentChanged),
            "an edit to still-readable contents is a ContentChanged conflict"
        );
    }

    #[test]
    fn an_unchanged_file_is_not_flagged() {
        // Accept criterion (no false positives): a file left untouched between
        // record and check produces no conflict.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(dir.path(), "config.json", b"{\"dnd\": false}\n");

        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("record baseline");

        assert!(
            tracker.check_conflicts().is_empty(),
            "an untouched file must not be reported as a conflict"
        );
    }

    #[test]
    fn identical_contents_with_a_touched_mtime_is_not_a_conflict() {
        // The content hash is authoritative: bumping only the mtime (a `touch`)
        // while leaving the bytes identical must NOT be reported — otherwise the
        // app would nag about phantom edits after any tool that restamps mtimes.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(
            dir.path(),
            "settings.ini",
            b"[Settings]\ngtk-theme-name=Nordic\n",
        );

        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("record baseline");

        // Advance the mtime well into the future without altering the bytes.
        set_mtime(&path, SystemTime::now() + Duration::from_secs(3600));

        assert!(
            tracker.check_conflicts().is_empty(),
            "a mtime touch with identical contents must not be a conflict"
        );
    }

    #[test]
    fn rewriting_the_same_bytes_is_not_a_conflict() {
        // Re-writing a file with byte-identical contents (e.g. a no-op save) must
        // not be flagged, since the content hash is unchanged.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(dir.path(), "colors.conf", b"scheme = nord\n");

        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("record baseline");

        fs::write(&path, b"scheme = nord\n").expect("rewrite identical bytes");

        assert!(
            tracker.check_conflicts().is_empty(),
            "rewriting identical bytes must not be a conflict"
        );
    }

    #[test]
    fn a_content_change_with_an_unchanged_mtime_is_still_detected() {
        // The strongest guard on the conflict rule: even if the mtime is reset to
        // exactly its recorded value, a change in the bytes must still be caught,
        // because the hash — not the mtime — is authoritative.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(dir.path(), "hypridle.conf", b"timeout = 300\n");

        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("record baseline");
        let recorded_mtime = fs::metadata(&path)
            .expect("stat file")
            .modified()
            .expect("mtime should be available");

        // Change the contents (which bumps the mtime), then restore the mtime to
        // its recorded value so an mtime-only check would see no change at all.
        fs::write(&path, b"timeout = 600\n").expect("external edit");
        set_mtime(&path, recorded_mtime);

        let conflicts = tracker.check_conflicts();
        assert_eq!(
            conflicts.len(),
            1,
            "a content change must be caught even when the mtime is unchanged"
        );
        assert!(matches!(
            conflicts[0].reason(),
            ConflictReason::ContentChanged
        ));
    }

    #[test]
    fn a_deleted_file_is_reported_as_unreadable() {
        // A file that vanishes between record and check cannot be verified, so it
        // is reported (as Unreadable) rather than silently proceeding to a write.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(
            dir.path(),
            "monitors.conf",
            b"monitor = eDP-1,preferred,auto,1\n",
        );

        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("record baseline");

        fs::remove_file(&path).expect("delete the tracked file");

        let conflicts = tracker.check_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path(), path.as_path());
        // The conflict must carry the underlying cause so the pipeline can log it;
        // a delete surfaces as a `NotFound` open error.
        match conflicts[0].reason() {
            ConflictReason::Unreadable(error) => {
                assert_eq!(error.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("a deleted file must be reported as Unreadable, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_reported() {
        // A file whose read permission is revoked after recording is reported as
        // Unreadable. Guarded so it degrades to a no-op when the test happens to
        // run as root (where the mode is ignored and the open still succeeds),
        // rather than producing a spurious failure.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(dir.path(), "input.conf", b"kb_layout = us\n");

        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("record baseline");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
            .expect("revoke read permission");

        // Only assert if the file is genuinely unreadable now (i.e. not running as
        // a privileged user that bypasses the mode).
        if File::open(&path).is_err() {
            let conflicts = tracker.check_conflicts();
            assert_eq!(conflicts.len(), 1);
            assert!(matches!(
                conflicts[0].reason(),
                ConflictReason::Unreadable(_)
            ));
        }

        // Restore permissions so the temp dir can be cleaned up.
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
    }

    #[test]
    fn only_the_changed_file_among_several_is_reported() {
        // With several tracked files, exactly the changed one is reported and the
        // rest are not — no false positives, and the report is path-sorted.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let a = write_file(dir.path(), "a.conf", b"a\n");
        let b = write_file(dir.path(), "b.conf", b"b\n");
        let c = write_file(dir.path(), "c.conf", b"c\n");

        let mut tracker = FreshnessTracker::new();
        for path in [&a, &b, &c] {
            tracker.record(path).expect("record baseline");
        }

        fs::write(&b, b"b changed\n").expect("edit only b");

        let conflicts = tracker.check_conflicts();
        assert_eq!(conflicts.len(), 1, "only the edited file must be reported");
        assert_eq!(conflicts[0].path(), b.as_path());
    }

    #[test]
    fn re_recording_re_baselines_a_resolved_conflict() {
        // After a conflict is handled by reloading the file, re-recording it makes
        // the tracker treat the new contents as the baseline, clearing the
        // conflict — the reload path the store (task 4.2) relies on.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(dir.path(), "config.conf", b"v = 1\n");

        let mut tracker = FreshnessTracker::new();
        tracker.record(&path).expect("record baseline");

        fs::write(&path, b"v = 2\n").expect("external edit");
        assert_eq!(tracker.check_conflicts().len(), 1, "the edit is a conflict");

        // Re-read/re-record the new contents as the accepted baseline.
        tracker.record(&path).expect("re-baseline after reload");
        assert!(
            tracker.check_conflicts().is_empty(),
            "re-recording must clear the conflict"
        );
    }

    #[test]
    fn recording_a_missing_file_is_an_error() {
        // `record` is fallible so a racing deletion is a clean error, not a panic.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let missing = dir.path().join("does-not-exist.conf");

        let mut tracker = FreshnessTracker::new();
        assert!(
            tracker.record(&missing).is_err(),
            "recording a nonexistent file must return an error"
        );
        assert!(
            tracker.is_empty(),
            "a failed record must not track anything"
        );
    }

    #[test]
    fn record_bytes_baselines_from_supplied_bytes() {
        // `record_bytes` is the entry point task 4.2's store uses: it hands over the
        // exact bytes it already read. Confirm the baseline behaves like `record`'s:
        // (ii) bytes still matching on disk are not flagged, and (i) a later
        // external edit is caught as ContentChanged.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let contents = b"env = XCURSOR_SIZE,16\n";
        let path = write_file(dir.path(), "hyprland.conf", contents);

        let mut tracker = FreshnessTracker::new();
        tracker.record_bytes(&path, contents);
        assert!(!tracker.is_empty(), "recording must track the path");

        // (ii) The on-disk bytes still match what was recorded: no conflict.
        assert!(
            tracker.check_conflicts().is_empty(),
            "on-disk bytes matching the recorded ones must not be flagged"
        );

        // (i) An external edit after recording is detected.
        fs::write(&path, b"env = XCURSOR_SIZE,24\n").expect("external edit");
        let conflicts = tracker.check_conflicts();
        assert_eq!(conflicts.len(), 1, "the edited file must be reported");
        assert_eq!(conflicts[0].path(), path.as_path());
        assert!(matches!(
            conflicts[0].reason(),
            ConflictReason::ContentChanged
        ));
    }

    #[test]
    fn record_bytes_fingerprints_supplied_bytes_not_the_current_file() {
        // The whole reason `record_bytes` exists (over `record`): it baselines the
        // bytes the caller read, closing the gap where an external edit lands
        // between the store's read and the tracker's own read. Simulate that race
        // by handing `record_bytes` the (older) bytes the store read while the file
        // on disk already holds newer bytes. A check must then flag the file,
        // because the disk differs from the recorded (store-read) baseline. This
        // test would fail if `record_bytes` re-read the file the way `record` does.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let store_read = b"v = 1\n";
        let path = write_file(dir.path(), "config.conf", b"v = 2\n");

        let mut tracker = FreshnessTracker::new();
        tracker.record_bytes(&path, store_read);

        let conflicts = tracker.check_conflicts();
        assert_eq!(
            conflicts.len(),
            1,
            "record_bytes must baseline the supplied bytes, so a disk that already \
             differs from them is a conflict"
        );
        assert!(matches!(
            conflicts[0].reason(),
            ConflictReason::ContentChanged
        ));
    }

    #[test]
    fn multiple_conflicts_are_reported_in_path_sorted_order() {
        // The task's *Done note promises path-sorted `check_conflicts()` output.
        // With several files changed at once, the returned conflicts must be in
        // ascending path order regardless of the order the files were recorded — so
        // record them deliberately unsorted, then change more than one.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let a = write_file(dir.path(), "a.conf", b"a\n");
        let b = write_file(dir.path(), "b.conf", b"b\n");
        let c = write_file(dir.path(), "c.conf", b"c\n");
        let d = write_file(dir.path(), "d.conf", b"d\n");

        let mut tracker = FreshnessTracker::new();
        for path in [&c, &a, &d, &b] {
            tracker.record(path).expect("record baseline");
        }

        // Change three of the four; `b` is left untouched.
        fs::write(&a, b"a changed\n").expect("edit a");
        fs::write(&c, b"c changed\n").expect("edit c");
        fs::write(&d, b"d changed\n").expect("edit d");

        let conflicts = tracker.check_conflicts();
        let paths: Vec<&Path> = conflicts.iter().map(Conflict::path).collect();
        assert_eq!(
            paths,
            vec![a.as_path(), c.as_path(), d.as_path()],
            "exactly the changed files must be reported, in ascending path order"
        );
        // Assert sortedness directly too, so the test still means something if the
        // filenames above are ever changed: no pair is out of order.
        assert!(
            paths.windows(2).all(|pair| pair[0] <= pair[1]),
            "the conflict list must be sorted by path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_edit_behind_a_symlink_is_detected() {
        // `record` follows symlinks (R8.5): tracking the live path — which in the
        // dotfiles deployment is a symlink into the repo — must catch an edit to the
        // real file behind the link, exactly as if the link were the file itself.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = write_file(dir.path(), "colors.real", b"scheme = nord\n");
        let link = dir.path().join("colors.conf");
        symlink(&target, &link).expect("create a symlink to the target");

        let mut tracker = FreshnessTracker::new();
        // Track the symlink path, the way the app addresses the live XDG path.
        tracker
            .record(&link)
            .expect("record baseline via the symlink");

        // Edit the real file behind the link, not the link path.
        fs::write(&target, b"scheme = gruvbox\n").expect("edit the symlink target");

        let conflicts = tracker.check_conflicts();
        assert_eq!(
            conflicts.len(),
            1,
            "an edit behind the symlink must be caught"
        );
        assert_eq!(
            conflicts[0].path(),
            link.as_path(),
            "the tracked (symlink) path is reported, not the resolved target"
        );
        assert!(matches!(
            conflicts[0].reason(),
            ConflictReason::ContentChanged
        ));
    }
}
