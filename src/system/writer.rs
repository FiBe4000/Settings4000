//! Atomic, symlink-following file writer (task 2.2, architecture §3 "Write
//! safety"; R5.4, R8.5).
//!
//! This is the single primitive the Apply pipeline (task 4.5) uses to persist an
//! edited config file. It exists to make one operation impossible to get subtly
//! wrong across the many backing files the app owns: replacing a file's contents
//! **atomically**, **following symlinks**, and **without ever leaving a partial
//! file** if anything fails partway through.
//!
//! # Addressing (R8.5)
//!
//! The caller always passes the file's *live* runtime path — the location under
//! `$XDG_CONFIG_HOME`/`~/.config` (or the equivalent per-app path) that the app
//! reads and the desktop actually loads — **never** a hardcoded `~/.dotfiles`
//! path. Whether that path is a plain file or a symlink into a dotfiles repo is
//! resolved here, so the writer behaves identically for a symlink-deployed
//! dotfiles setup and for a user with ordinary config files.
//!
//! # Flow (R5.4)
//!
//! 1. **Canonicalize** the target with [`std::fs::canonicalize`]. A symlink into
//!    a dotfiles repo resolves to the real repo file; a plain file resolves to
//!    itself. Because canonicalize requires the path to exist, this writer is for
//!    rewriting existing backing files — a missing target is a clean
//!    [`WriteError::Resolve`], not a panic.
//! 2. **Snapshot** the resolved file's current bytes in memory so the Apply
//!    pipeline can roll this file back if a *later* file in the same Apply fails
//!    (see [`FileSnapshot::restore`]).
//! 3. Stage the new content in a [`tempfile::NamedTempFile`] created **in the
//!    resolved target's own directory**, so the final rename is a same-filesystem
//!    atomic operation (a cross-device rename would fail).
//! 4. **fsync** the temp file, then **atomically rename** it over the resolved
//!    target. Renaming over the *resolved* path — not the symlink — rewrites the
//!    real file's bytes and leaves any symlink pointing at it untouched, so the
//!    link is preserved rather than replaced by a regular file.
//! 5. Best-effort **fsync of the parent directory** so the rename itself is
//!    durable across a crash.
//!
//! Because the new content only ever reaches the real file through the final
//! atomic rename, any failure before that step leaves the original file
//! byte-for-byte intact, and the `NamedTempFile` is deleted on drop so no stray
//! temp file is left behind.

use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

/// A failure while snapshotting or atomically writing a backing file.
///
/// The variants mark *where* the operation stopped so the Apply pipeline can
/// report a precise cause. In every case the real file is left in its previous
/// state: `Resolve` and `Snapshot` happen before any temp file is created, and a
/// `Write` failure discards the temp file without touching the target (the new
/// bytes only reach the target through the final atomic rename).
#[derive(Debug)]
pub(crate) enum WriteError {
    /// The target's real path could not be resolved with
    /// [`std::fs::canonicalize`] — most commonly because it does not exist. No
    /// write was attempted and nothing was created.
    Resolve {
        /// The live path the caller asked to write.
        target: PathBuf,
        /// The underlying OS error from canonicalization.
        source: io::Error,
    },
    /// The resolved file's current bytes could not be read to snapshot it for
    /// rollback (e.g. it is unreadable). No write was attempted.
    Snapshot {
        /// The canonicalized path whose bytes could not be read.
        path: PathBuf,
        /// The underlying OS error from the read.
        source: io::Error,
    },
    /// The atomic write itself failed — creating, writing, or syncing the temp
    /// file, or the final rename. The original file is untouched and the temp
    /// file has been discarded.
    Write {
        /// The canonicalized path the write targeted.
        path: PathBuf,
        /// The underlying OS error.
        source: io::Error,
    },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Resolve { target, source } => {
                write!(f, "failed to resolve {}: {source}", target.display())
            }
            WriteError::Snapshot { path, source } => {
                write!(
                    f,
                    "failed to read {} for its pre-write snapshot: {source}",
                    path.display()
                )
            }
            WriteError::Write { path, source } => {
                write!(f, "failed to atomically write {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for WriteError {
    /// Exposes the underlying [`io::Error`] so callers that print or wrap the
    /// error get the full OS-level cause in the chain.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WriteError::Resolve { source, .. }
            | WriteError::Snapshot { source, .. }
            | WriteError::Write { source, .. } => Some(source),
        }
    }
}

/// The pre-write state of a file, kept in memory for per-file rollback.
///
/// [`write_atomic`] returns one of these on success, holding the resolved
/// (symlink-followed) path together with the file's bytes *and permission mode*
/// as they were before the write. The Apply pipeline (task 4.5) collects a
/// snapshot per successfully-written file and, if a later file in the same Apply
/// fails, calls [`FileSnapshot::restore`] on each to put the earlier files back —
/// so a partial Apply never leaves the desktop in a half-changed state (R5.4).
///
/// The snapshot deliberately captures the permission mode as well as the bytes so
/// that a rollback returns the file's *full* pre-write state (R8.3: never surprise
/// a working setup). If the forward write's best-effort permission preservation
/// had silently failed and left the file's mode tightened, restoring only the
/// bytes would keep that wrong mode; capturing the mode here lets
/// [`restore`](FileSnapshot::restore) reapply it faithfully rather than trusting
/// whatever mode happens to be on disk at rollback time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileSnapshot {
    /// The canonicalized real path these bytes were read from and are restored
    /// to. Storing the resolved path means [`restore`](FileSnapshot::restore)
    /// rewrites the same real file (preserving any symlink that points at it),
    /// without re-canonicalizing.
    resolved_path: PathBuf,
    /// The file's exact bytes at snapshot time, restored verbatim on rollback.
    contents: Vec<u8>,
    /// The file's permissions at snapshot time (on Unix, the `0644`-style mode
    /// bits), reapplied to the new content on rollback so the file's permissions
    /// are restored alongside its bytes. `None` when the permissions could not be
    /// read at snapshot time; the restore then falls back to the temp file's
    /// default permissions, exactly as the forward write does (R8.3).
    permissions: Option<fs::Permissions>,
}

impl FileSnapshot {
    /// The canonicalized real path this snapshot was taken from.
    pub(crate) fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    /// The captured original bytes.
    pub(crate) fn contents(&self) -> &[u8] {
        &self.contents
    }

    /// Restores the file to its snapshotted contents and permission mode,
    /// atomically.
    ///
    /// Used by the Apply pipeline's per-file rollback (task 4.5): after a later
    /// file in the same Apply fails, each already-written file is put back with
    /// this. The restore uses the identical atomic temp→fsync→rename mechanism as
    /// the forward write and targets the stored resolved path, so a symlinked
    /// file's link is preserved here too. Reapplying the snapshotted mode is
    /// best-effort, matching the forward write: a chmod failure logs at `debug`
    /// and does not fail the restore (R8.3).
    pub(crate) fn restore(&self) -> Result<(), WriteError> {
        // Restore both the bytes and the snapshotted permission mode, so rollback
        // returns the file's full pre-write state rather than trusting the mode
        // currently on disk (which may have been tightened, see the type docs).
        atomic_write_bytes(
            &self.resolved_path,
            &self.contents,
            self.permissions.clone(),
        )?;
        // R7.3: log the rollback write with its resolved path (byte count only,
        // never contents). The "why we rolled back" context belongs to the Apply
        // orchestrator (task 4.5).
        tracing::info!(
            path = %self.resolved_path.display(),
            bytes = self.contents.len(),
            "restored file to its pre-write contents"
        );
        Ok(())
    }
}

/// Atomically replaces `target`'s contents with `contents`, following symlinks.
///
/// Resolves `target` to its real path (a symlink into a dotfiles repo resolves to
/// the repo file; a plain file resolves to itself), snapshots the current bytes,
/// then writes `contents` via a temp file in the resolved directory that is
/// fsynced and atomically renamed over the resolved target (R5.4). The returned
/// [`FileSnapshot`] holds the *original* bytes so the caller can roll this file
/// back if a later step of the same Apply fails.
///
/// On any failure the original file is left untouched and no partial or temp file
/// remains. `target` must exist (canonicalization requires it); a missing path is
/// a [`WriteError::Resolve`].
pub(crate) fn write_atomic(target: &Path, contents: &[u8]) -> Result<FileSnapshot, WriteError> {
    let resolved = resolve_target(target)?;

    // Capture the pre-write bytes before touching anything, so rollback has the
    // exact original to restore.
    let original = fs::read(&resolved).map_err(|source| WriteError::Snapshot {
        path: resolved.clone(),
        source,
    })?;

    // Capture the pre-write permissions too, so the snapshot represents the file's
    // full prior state: the forward write preserves this mode onto the new content
    // (so the swap does not tighten a 0644 config to the temp file's 0600), and a
    // rollback reapplies it. Best-effort — see `capture_permissions`.
    let original_permissions = capture_permissions(&resolved);

    atomic_write_bytes(&resolved, contents, original_permissions.clone())?;

    // R7.3: log the write with the resolved path and byte count only. Which keys
    // changed is logged by the Apply orchestrator (task 4.5); full contents are
    // never logged.
    tracing::info!(
        path = %resolved.display(),
        bytes = contents.len(),
        "wrote configuration file atomically"
    );

    Ok(FileSnapshot {
        resolved_path: resolved,
        contents: original,
        permissions: original_permissions,
    })
}

/// Canonicalizes `target` to the real path whose bytes will be rewritten.
///
/// [`std::fs::canonicalize`] resolves every symlink in the path including the
/// final component, so a config file deployed as a symlink into a dotfiles repo
/// resolves to the repo file (whose bytes we then rewrite, leaving the link in
/// place), and a plain file resolves to itself.
fn resolve_target(target: &Path) -> Result<PathBuf, WriteError> {
    fs::canonicalize(target).map_err(|source| WriteError::Resolve {
        target: target.to_path_buf(),
        source,
    })
}

/// Atomically writes `contents` to an already-resolved path, applying
/// `permissions` to the new file.
///
/// Shared by [`write_atomic`] (forward write) and [`FileSnapshot::restore`]
/// (rollback); both hand it a canonicalized target, so it never resolves symlinks
/// itself. `permissions` are the mode to stamp onto the replacement file — the
/// original file's mode in both directions — or `None` to keep the temp file's
/// default (see [`apply_permissions`]).
fn atomic_write_bytes(
    resolved: &Path,
    contents: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<(), WriteError> {
    atomic_write_with(resolved, permissions, |file| file.write_all(contents))
}

/// The atomic-write core: stage into a temp file, fsync, and rename over the
/// resolved target, with the content-producing step supplied as `fill`.
///
/// Factoring the content step out as a closure is the deliberate **test seam**
/// for the "no partial file on failure" guarantee (R5.4): production passes a
/// closure that writes the new bytes, while a test can pass one that returns an
/// error to prove that a failure before the rename leaves the original file
/// intact and no temp file behind. The [`NamedTempFile`] is created in the
/// resolved target's own directory so the rename is a same-filesystem atomic
/// operation, and it is deleted on drop, so every early-return path cleans up the
/// temp file automatically.
fn atomic_write_with<F>(
    resolved: &Path,
    permissions: Option<fs::Permissions>,
    fill: F,
) -> Result<(), WriteError>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    let dir = resolved.parent().ok_or_else(|| WriteError::Write {
        path: resolved.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "resolved path has no parent directory to place the temp file in",
        ),
    })?;

    // Create the temp file beside the real target. On any early return below the
    // `NamedTempFile` is dropped and the temp file deleted, so a failure never
    // leaves a stray partial file.
    let mut temp = NamedTempFile::new_in(dir).map_err(|source| WriteError::Write {
        path: resolved.to_path_buf(),
        source,
    })?;

    fill(temp.as_file_mut()).map_err(|source| WriteError::Write {
        path: resolved.to_path_buf(),
        source,
    })?;

    // Stamp the original file's permissions onto the replacement: a fresh temp
    // file defaults to 0600, which would silently tighten a typical 0644 config
    // file when it is swapped in. Best-effort — a config write must not fail over
    // this nicety (R8.3: never surprise a working setup) — so a failure only logs
    // at `debug`.
    apply_permissions(resolved, &temp, permissions);

    // Flush the new bytes (and the permission change) to stable storage before
    // the rename, so a crash cannot expose an empty or half-written file.
    temp.as_file()
        .sync_all()
        .map_err(|source| WriteError::Write {
            path: resolved.to_path_buf(),
            source,
        })?;

    // The atomic step: rename the fully-written temp file over the resolved
    // target. `rename(2)` is atomic within a filesystem, so a reader only ever
    // sees either the whole old file or the whole new one. If it fails, the returned
    // `PersistError` (dropped here) carries the temp file, which is deleted.
    temp.persist(resolved)
        .map_err(|persist_error| WriteError::Write {
            path: resolved.to_path_buf(),
            source: persist_error.error,
        })?;

    // Durability of the rename itself needs the *directory* fsynced; best-effort,
    // since the new content is already the live file on a running system.
    fsync_dir(dir);

    Ok(())
}

/// Reads the resolved target's current permissions to reuse when writing.
///
/// The captured value is used twice: the forward write stamps it onto the new
/// content (so swapping in a temp file does not tighten a typical 0644 config to
/// the temp file's default 0600), and it is stored in the [`FileSnapshot`] so a
/// rollback can reapply it. Best-effort: if the metadata cannot be read the reason
/// is logged at `debug` and `None` is returned, so the write proceeds with the
/// temp file's default permissions rather than failing an otherwise-valid config
/// write over a permissions nicety (R8.3).
fn capture_permissions(resolved: &Path) -> Option<fs::Permissions> {
    match fs::metadata(resolved) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) => {
            tracing::debug!(
                path = %resolved.display(),
                error = %error,
                "could not read the original file permissions to preserve; writing with default permissions"
            );
            None
        }
    }
}

/// Applies previously-[captured](capture_permissions) permissions to the staged
/// temp file, before it is renamed into place.
///
/// Best-effort and consistent with the rest of the permission handling: if the
/// permissions cannot be set, the temp file's defaults are kept and the reason is
/// logged at `debug`, never failing the write (R8.3). `None` means the original
/// permissions could not be captured, so nothing is applied and the temp file's
/// defaults stand.
fn apply_permissions(target: &Path, temp: &NamedTempFile, permissions: Option<fs::Permissions>) {
    let Some(permissions) = permissions else {
        return;
    };
    if let Err(error) = temp.as_file().set_permissions(permissions) {
        tracing::debug!(
            path = %target.display(),
            error = %error,
            "could not apply the original file permissions to the temp file; writing with default permissions"
        );
    }
}

/// Fsyncs a directory so a rename within it is durable across a crash.
///
/// Best-effort: the rename has already made the new content the live file, so a
/// failure to fsync the directory is a durability shortfall (the change might not
/// survive a power loss), not a correctness problem for the running desktop — it
/// is therefore logged at `debug` rather than surfaced as a write failure.
fn fsync_dir(dir: &Path) {
    match File::open(dir) {
        Ok(handle) => {
            if let Err(error) = handle.sync_all() {
                tracing::debug!(
                    dir = %dir.display(),
                    error = %error,
                    "could not fsync the parent directory after rename"
                );
            }
        }
        Err(error) => {
            tracing::debug!(
                dir = %dir.display(),
                error = %error,
                "could not open the parent directory to fsync after rename"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_atomic_rewrites_symlink_target_and_preserves_the_link() {
        // R5.4/R8.5: the app addresses the live XDG path, which for this
        // dotfiles setup is a symlink into the repo. Writing through it must
        // rewrite the *real* repo file's bytes and leave the link intact.
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().expect("repo temp dir should be creatable");
        let xdg = tempfile::tempdir().expect("xdg temp dir should be creatable");

        // The real backing file lives in the "dotfiles repo".
        let real = repo.path().join("colors.conf");
        fs::write(&real, b"scheme = nord\n").expect("write real file");

        // The XDG path the app addresses is a symlink into the repo.
        let link = xdg.path().join("colors.conf");
        symlink(&real, &link).expect("create symlink into the repo");

        let snapshot =
            write_atomic(&link, b"scheme = everforest\n").expect("atomic write via the symlink");

        // The real target's bytes were rewritten...
        assert_eq!(
            fs::read(&real).expect("read real file"),
            b"scheme = everforest\n"
        );
        // ...the snapshot resolved through the symlink and captured the original
        // bytes for rollback...
        assert_eq!(
            snapshot.resolved_path(),
            fs::canonicalize(&real)
                .expect("canonicalize real file")
                .as_path()
        );
        assert_eq!(snapshot.contents(), b"scheme = nord\n");
        // ...and the XDG path is still a symlink pointing at the same real file
        // (never replaced by a regular file).
        let link_meta = fs::symlink_metadata(&link).expect("lstat the link");
        assert!(
            link_meta.file_type().is_symlink(),
            "the XDG path must remain a symlink after the write"
        );
        assert_eq!(
            fs::read_link(&link).expect("read the link target"),
            real,
            "the link must still point at the same real file"
        );

        // Rolling back via the snapshot restores the original bytes to the real
        // target and still preserves the link.
        snapshot.restore().expect("restore via the snapshot");
        assert_eq!(
            fs::read(&real).expect("read real file after restore"),
            b"scheme = nord\n"
        );
        assert!(
            fs::symlink_metadata(&link)
                .expect("lstat the link after restore")
                .file_type()
                .is_symlink(),
            "the link must survive a rollback too"
        );
    }

    #[test]
    fn write_atomic_rewrites_a_plain_file_in_place() {
        // R5.4: a plain (non-symlink) file resolves to itself and is rewritten in
        // place — the same path holds the new bytes afterward.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = dir.path().join("settings.ini");
        fs::write(&target, b"[Settings]\ngtk-theme-name=Adwaita\n").expect("write plain file");

        let snapshot = write_atomic(&target, b"[Settings]\ngtk-theme-name=Nordic\n")
            .expect("atomic write in place");

        assert_eq!(
            fs::read(&target).expect("read file"),
            b"[Settings]\ngtk-theme-name=Nordic\n"
        );
        // The atomic rename swaps the file behind the path (so the inode changes),
        // but the path the app addressed is unchanged and is still a regular
        // file — never turned into a symlink.
        let meta = fs::symlink_metadata(&target).expect("lstat the target");
        assert!(meta.file_type().is_file());
        assert!(!meta.file_type().is_symlink());
        assert_eq!(
            snapshot.resolved_path(),
            fs::canonicalize(&target)
                .expect("canonicalize target")
                .as_path()
        );
    }

    #[test]
    fn write_leaves_no_partial_file_or_temp_on_injected_failure() {
        // R5.4: an injected failure before the rename must leave the original
        // untouched and no stray temp file. `atomic_write_with` is the test seam:
        // production supplies a closure that writes the bytes; here it fails.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = dir.path().join("config.conf");
        fs::write(&target, b"original\n").expect("write original file");
        let resolved = fs::canonicalize(&target).expect("canonicalize target");

        let result = atomic_write_with(&resolved, None, |_file| {
            Err(io::Error::other("injected write failure"))
        });
        assert!(
            matches!(result, Err(WriteError::Write { .. })),
            "an injected fill failure must surface as a Write error, got {result:?}"
        );

        // The original file is byte-for-byte intact.
        assert_eq!(
            fs::read(&resolved).expect("read the untouched original"),
            b"original\n"
        );
        // No leftover temp file: the target is the only entry in the directory.
        let entries: Vec<_> = fs::read_dir(dir.path())
            .expect("read the directory")
            .map(|entry| entry.expect("read a dir entry").file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("config.conf")],
            "a failed write must not leave a temp file behind"
        );
    }

    #[test]
    fn snapshot_restore_returns_original_contents() {
        // R5.4: the pre-write snapshot restores the file to exactly its original
        // bytes — the per-file rollback the Apply pipeline relies on.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = dir.path().join("config.json");
        fs::write(&target, b"{\"n\":1}\n").expect("write original file");

        let snapshot = write_atomic(&target, b"{\"n\":2}\n").expect("atomic write");
        assert_eq!(fs::read(&target).expect("read after write"), b"{\"n\":2}\n");
        assert_eq!(snapshot.contents(), b"{\"n\":1}\n");

        snapshot.restore().expect("restore original contents");
        assert_eq!(
            fs::read(&target).expect("read after restore"),
            b"{\"n\":1}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_restore_reapplies_the_original_mode() {
        // R5.4/R8.3: a snapshot must capture the file's full pre-write state, its
        // permission mode included, so a rollback returns it faithfully. This
        // guards the pathological case where the mode was tightened after the
        // forward write (as if the best-effort permission preservation had
        // silently failed): the restore must reapply the *snapshotted* mode, not
        // keep whatever mode happens to be on disk at rollback time.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = dir.path().join("config.conf");
        fs::write(&target, b"original\n").expect("write original file");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("chmod 0644");

        let snapshot = write_atomic(&target, b"changed\n").expect("atomic write");
        assert_eq!(fs::read(&target).expect("read after write"), b"changed\n");

        // Simulate the mode drifting to a tightened 0600 after the forward write,
        // so a bytes-only restore would leave the file wrongly locked down.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("tighten to 0600");

        snapshot
            .restore()
            .expect("restore original contents and mode");

        assert_eq!(
            fs::read(&target).expect("read after restore"),
            b"original\n"
        );
        let mode = fs::metadata(&target)
            .expect("stat restored file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o644,
            "restore must reapply the snapshotted mode, not the tightened on-disk mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_the_original_file_mode() {
        // R8.3: the writer must not silently change a config file's permissions
        // when it swaps in the new content. A fresh temp file defaults to 0600,
        // so without preservation a typical 0644 config would be tightened.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = dir.path().join("hyprland.conf");
        fs::write(&target, b"env = XCURSOR_THEME,Nordic-cursors\n").expect("write file");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("chmod 0644");

        write_atomic(
            &target,
            b"env = XCURSOR_THEME,Nordic-cursors\nenv = XCURSOR_SIZE,16\n",
        )
        .expect("atomic write");

        let mode = fs::metadata(&target)
            .expect("stat rewritten file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o644,
            "the rewritten file must keep the original mode"
        );
    }

    #[test]
    fn write_atomic_on_a_missing_target_errors_without_writing() {
        // The writer rewrites existing backing files: it canonicalizes first
        // (architecture §3), which requires the target to exist. A missing path
        // is a clean `Resolve` error rather than a panic, and creates nothing.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let missing = dir.path().join("does-not-exist.conf");

        let result = write_atomic(&missing, b"data\n");
        assert!(
            matches!(result, Err(WriteError::Resolve { .. })),
            "a missing target must be a Resolve error, got {result:?}"
        );
        assert!(!missing.exists(), "no file should have been created");
        assert_eq!(
            fs::read_dir(dir.path())
                .expect("read the directory")
                .count(),
            0,
            "nothing should have been created in the directory"
        );
    }
}
