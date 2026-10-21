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
//!
//! # Creating a file that does not exist yet
//!
//! [`write_atomic`] only ever *rewrites*: a missing target is an error, which is
//! the right default because every backing file the app edits is expected to
//! exist, and silently creating one at a mistyped path would hide a bug. One
//! write target is different: the XCursor fallback theme
//! `~/.icons/default/index.theme` (analysis §8.4) belongs to the R3.4 cursor write
//! set but is legitimately absent on a host without the dotfiles deployment, and
//! the app then creates it. [`create_atomic`] is that explicit, separate entry
//! point, with the same guarantees turned around:
//!
//! - It is **exclusive**: it refuses with [`WriteError::AlreadyExists`] when
//!   anything is at the target — a file, a directory, or a symlink, dangling or
//!   not — so it can never clobber a file another program created since the app
//!   looked, and never replaces a symlink with a plain file. The final step is a
//!   no-clobber rename (`persist_noclobber`), so this also holds against a file
//!   that appears in the window between that check and the rename.
//! - It **creates missing parent directories** as plain directories (as the
//!   dotfiles' `setup.sh` does with `mkdir -p ~/.icons/default`). An existing
//!   parent that is a symlink is followed, so the file lands in the directory it
//!   points to.
//! - It is **atomic** in the same way: the content is staged in a temp file in
//!   the (resolved) parent directory, fsynced, and only then given its name.
//! - Its [`FileSnapshot`] records that the file was **absent**, so a rollback
//!   deletes the created file and removes the parent directories this call
//!   created (only while they are still empty) — the pre-write state was "no
//!   file", and restoring it means exactly that.

use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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
pub enum WriteError {
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
    /// [`create_atomic`] found something already at the path it was asked to
    /// create (a file, a directory, or a symlink). Creation is exclusive, so
    /// nothing was written: an existing entry is either another program's file,
    /// which must not be clobbered (R5.6), or a symlink, which must never be
    /// replaced by a plain file.
    AlreadyExists {
        /// The path that was to be created.
        target: PathBuf,
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
            WriteError::AlreadyExists { target } => write!(
                f,
                "refused to create {}: something already exists at that path",
                target.display()
            ),
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
            WriteError::AlreadyExists { .. } => None,
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
///
/// A snapshot returned by [`create_atomic`] instead records that there was **no
/// file**; restoring it deletes the created file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSnapshot {
    /// The canonicalized real path these bytes were read from and are restored
    /// to. Storing the resolved path means [`restore`](FileSnapshot::restore)
    /// rewrites the same real file (preserving any symlink that points at it),
    /// without re-canonicalizing.
    resolved_path: PathBuf,
    /// What the path held before the write.
    prior: PriorState,
}

/// What a write target held before the write, so [`FileSnapshot::restore`]
/// returns it to exactly that state.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PriorState {
    /// The file existed ([`write_atomic`]).
    Existed {
        /// The file's exact bytes at snapshot time, restored verbatim on rollback.
        contents: Vec<u8>,
        /// The file's permissions at snapshot time (on Unix, the `0644`-style
        /// mode bits), reapplied to the new content on rollback so the file's
        /// permissions are restored alongside its bytes. `None` when the
        /// permissions could not be read at snapshot time; the restore then falls
        /// back to the temp file's default permissions, exactly as the forward
        /// write does (R8.3).
        permissions: Option<fs::Permissions>,
    },
    /// The file did not exist and [`create_atomic`] created it.
    Absent {
        /// The bytes [`create_atomic`] wrote. A rollback deletes the file only
        /// while it still holds exactly these bytes, so a file another program
        /// rewrote in the meantime is never deleted.
        written: Vec<u8>,
        /// The parent directories that call created because they were missing
        /// too, outermost first. A rollback removes them innermost first, and only
        /// while they are empty, so it never deletes anything it did not create.
        created_dirs: Vec<PathBuf>,
    },
}

impl FileSnapshot {
    /// The canonicalized real path this snapshot was taken from.
    pub fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    /// The captured original bytes, or `None` when the write created the file
    /// (there were no original bytes to capture).
    pub fn contents(&self) -> Option<&[u8]> {
        match &self.prior {
            PriorState::Existed { contents, .. } => Some(contents),
            PriorState::Absent { .. } => None,
        }
    }

    /// Restores the file to its snapshotted state, atomically.
    ///
    /// Used by the Apply pipeline's per-file rollback (task 4.5): after a later
    /// file in the same Apply fails, each already-written file is put back with
    /// this.
    ///
    /// For a file that existed, the restore uses the identical atomic
    /// temp→fsync→rename mechanism as the forward write and targets the stored
    /// resolved path, so a symlinked file's link is preserved here too.
    /// Reapplying the snapshotted mode is best-effort, matching the forward write:
    /// a chmod failure logs at `debug` and does not fail the restore (R8.3).
    ///
    /// For a file [`create_atomic`] created, restoring means removing it (a file
    /// that is already gone counts as restored) and then removing the parent
    /// directories that call created, innermost first. The file is removed only
    /// while it still holds exactly the bytes the creation wrote: if another program
    /// has rewritten it since, it is no longer the app's file, so it is left in place
    /// and the skipped removal logged at `warn` (R5.6 — never clobber an external
    /// change, and deleting is the strongest clobber there is). Directory removal is
    /// best-effort: `remove_dir` only removes an empty directory, so one that has
    /// meanwhile gained other content is left alone and logged at `debug`.
    pub fn restore(&self) -> Result<(), WriteError> {
        match &self.prior {
            PriorState::Existed {
                contents,
                permissions,
            } => {
                // Restore both the bytes and the snapshotted permission mode, so
                // rollback returns the file's full pre-write state rather than
                // trusting the mode currently on disk (which may have been
                // tightened, see the type docs).
                atomic_write_bytes(&self.resolved_path, contents, permissions.clone())?;
                // R7.3: log the rollback write with its resolved path (byte count
                // only, never contents). The "why we rolled back" context belongs
                // to the Apply orchestrator (task 4.5).
                tracing::info!(
                    path = %self.resolved_path.display(),
                    bytes = contents.len(),
                    "restored file to its pre-write contents"
                );
            }
            PriorState::Absent {
                written,
                created_dirs,
            } => {
                match fs::read(&self.resolved_path) {
                    Ok(current) if current == *written => {
                        match fs::remove_file(&self.resolved_path) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(source) => {
                                return Err(WriteError::Write {
                                    path: self.resolved_path.clone(),
                                    source,
                                });
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        tracing::warn!(
                            path = %self.resolved_path.display(),
                            "the file this apply created was changed by another program \
                             before the rollback; leaving it in place"
                        );
                        return Ok(());
                    }
                    Err(source) => {
                        return Err(WriteError::Snapshot {
                            path: self.resolved_path.clone(),
                            source,
                        });
                    }
                }
                if let Some(dir) = self.resolved_path.parent() {
                    fsync_dir(dir);
                }
                remove_created_dirs(created_dirs);
                tracing::info!(
                    path = %self.resolved_path.display(),
                    "removed the file this apply had created"
                );
            }
        }
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
pub fn write_atomic(target: &Path, contents: &[u8]) -> Result<FileSnapshot, WriteError> {
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
        prior: PriorState::Existed {
            contents: original,
            permissions: original_permissions,
        },
    })
}

/// The permission mode given to a file [`create_atomic`] creates: owner
/// read/write, everyone else read — what an ordinary config file gets under the
/// usual `022` umask. Set explicitly because the staging temp file is created
/// `0600`, which would otherwise make the new file unreadable to other users for
/// no reason (the files the dotfiles deploy are all `0644`).
///
/// The mode is applied with `chmod` after the temp file exists, so it holds
/// **regardless of the process umask** — a stricter umask (say `077`) is not
/// honoured. That is acceptable here: the only file created this way is
/// `~/.icons/default/index.theme`, which holds nothing but a cursor theme name and
/// must be readable by every program in the session; the parent directories are
/// created with `create_dir` and do follow the umask.
const NEW_FILE_MODE: u32 = 0o644;

/// Atomically creates `target` with `contents`, refusing if anything already
/// exists at that path (see the module section "Creating a file that does not
/// exist yet").
///
/// Missing parent directories are created as plain directories; an existing
/// symlinked parent is followed, so the returned snapshot's
/// [`resolved_path`](FileSnapshot::resolved_path) is the real location. The new
/// file gets mode `0644` (see `NEW_FILE_MODE`). On success the [`FileSnapshot`] records the
/// file as absent, so [`FileSnapshot::restore`] deletes it again (with the
/// directories this call created).
///
/// Errors: [`WriteError::AlreadyExists`] when the target exists (checked first,
/// and again atomically by the final no-clobber rename); [`WriteError::Write`]
/// when a directory, the temp file or the rename fails — including a parent path
/// that runs through a dangling symlink: the directory walk stops at the link, and
/// `create_dir` below it follows the link and fails with `NotFound`, before any
/// canonicalization; [`WriteError::Resolve`] when the parent directory cannot be
/// canonicalized after the directories exist. On every error nothing is left
/// behind: directories this call created are removed again, and no symlink is ever
/// replaced.
pub fn create_atomic(target: &Path, contents: &[u8]) -> Result<FileSnapshot, WriteError> {
    // `symlink_metadata` does not follow a final symlink, so a dangling link
    // counts as "something is here" too — it must never be replaced.
    if fs::symlink_metadata(target).is_ok() {
        return Err(WriteError::AlreadyExists {
            target: target.to_path_buf(),
        });
    }
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(WriteError::Write {
            path: target.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path to create has no parent directory or file name",
            ),
        });
    };

    let created_dirs = create_missing_dirs(parent).map_err(|source| WriteError::Write {
        path: target.to_path_buf(),
        source,
    })?;

    let resolved = match fs::canonicalize(parent) {
        Ok(dir) => dir.join(name),
        Err(source) => {
            remove_created_dirs(&created_dirs);
            return Err(WriteError::Resolve {
                target: target.to_path_buf(),
                source,
            });
        }
    };

    let permissions = Some(fs::Permissions::from_mode(NEW_FILE_MODE));
    if let Err(error) = atomic_write_with(&resolved, permissions, Persist::NoClobber, |file| {
        file.write_all(contents)
    }) {
        remove_created_dirs(&created_dirs);
        return Err(error);
    }

    // R7.3: path, byte count and how many directories were needed — never contents.
    tracing::info!(
        path = %resolved.display(),
        bytes = contents.len(),
        created_dirs = created_dirs.len(),
        "created configuration file atomically"
    );

    Ok(FileSnapshot {
        resolved_path: resolved,
        prior: PriorState::Absent {
            written: contents.to_vec(),
            created_dirs,
        },
    })
}

/// Creates every missing directory on the way to `dir`, outermost first, and
/// returns the ones it created (outermost first).
///
/// Walks up from `dir` with `symlink_metadata` until it meets an existing entry,
/// so an existing symlink — dangling or not — ends the walk instead of being
/// replaced by a directory. Below a dangling link, the first `create_dir` follows
/// the link to its missing target and fails with `NotFound`, so the caller reports
/// a [`WriteError::Write`] with the link untouched. Each directory is created individually with
/// `create_dir` rather than `create_dir_all`, so the list of created directories
/// is exact; on a failure partway, the ones already created are removed again.
fn create_missing_dirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    let mut current = Some(dir);
    while let Some(path) = current {
        if path.as_os_str().is_empty() {
            break;
        }
        match fs::symlink_metadata(path) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(path.to_path_buf());
                current = path.parent();
            }
            Err(error) => return Err(error),
        }
    }
    missing.reverse();
    for (index, path) in missing.iter().enumerate() {
        if let Err(error) = fs::create_dir(path) {
            remove_created_dirs(&missing[..index]);
            return Err(error);
        }
    }
    Ok(missing)
}

/// Removes directories a [`create_atomic`] call created, innermost first.
///
/// Best-effort and conservative: `remove_dir` only removes an *empty*
/// directory, so if something else has been put inside one meanwhile it stays,
/// and the failure is logged at `debug` — leaving an empty-looking directory
/// behind is harmless, deleting someone's file would not be.
fn remove_created_dirs(created_dirs: &[PathBuf]) {
    for dir in created_dirs.iter().rev() {
        if let Err(error) = fs::remove_dir(dir) {
            tracing::debug!(
                dir = %dir.display(),
                %error,
                "could not remove a directory created for a new file; leaving it"
            );
        }
    }
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
    atomic_write_with(resolved, permissions, Persist::Replace, |file| {
        file.write_all(contents)
    })
}

/// How [`atomic_write_with`] gives the staged temp file its final name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Persist {
    /// `rename(2)` over the existing target — the rewrite path.
    Replace,
    /// Fail if the target exists (`persist_noclobber`) — the [`create_atomic`]
    /// path, which must never clobber a file that appeared after its existence
    /// check.
    NoClobber,
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
    persist: Persist,
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
    // `PersistError` (dropped here) carries the temp file, which is deleted. The
    // no-clobber form gives a file its name only if none exists, atomically.
    let persisted = match persist {
        Persist::Replace => temp.persist(resolved),
        Persist::NoClobber => temp.persist_noclobber(resolved),
    };
    persisted.map_err(|persist_error| {
        if persist == Persist::NoClobber
            && persist_error.error.kind() == io::ErrorKind::AlreadyExists
        {
            WriteError::AlreadyExists {
                target: resolved.to_path_buf(),
            }
        } else {
            WriteError::Write {
                path: resolved.to_path_buf(),
                source: persist_error.error,
            }
        }
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
        assert_eq!(snapshot.contents(), Some(&b"scheme = nord\n"[..]));
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

        let result = atomic_write_with(&resolved, None, Persist::Replace, |_file| {
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
        assert_eq!(snapshot.contents(), Some(&b"{\"n\":1}\n"[..]));

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
        let target = dir.path().join("hypridle.conf");
        fs::write(&target, b"general {\n}\n").expect("write file");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("chmod 0644");

        write_atomic(&target, b"general {\n    lock_cmd = hyprlock\n}\n").expect("atomic write");

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

    #[test]
    fn create_atomic_creates_the_file_and_its_missing_parents_and_restore_removes_them() {
        // The XCursor fallback theme is created when absent (task 11.5, analysis
        // §8.4): `~/.icons` may exist while `default/` does not. The file lands with
        // the given bytes and a normal 0644 mode, the snapshot records "absent", and
        // a rollback deletes the file plus exactly the directories it had to create
        // — never the pre-existing `icons` directory.
        let home = tempfile::tempdir().expect("temp dir should be creatable");
        let icons = home.path().join(".icons");
        fs::create_dir(&icons).expect("create the existing parent");
        let target = icons.join("default").join("nested").join("index.theme");

        let snapshot = create_atomic(&target, b"[Icon Theme]\n").expect("create the file");
        assert_eq!(fs::read(&target).expect("read created"), b"[Icon Theme]\n");
        assert_eq!(snapshot.contents(), None, "there were no original bytes");
        assert_eq!(
            snapshot.resolved_path(),
            fs::canonicalize(&target).expect("canonicalize").as_path()
        );
        let mode = fs::metadata(&target)
            .expect("stat created")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, NEW_FILE_MODE,
            "a created config file is 0644, not 0600"
        );
        let created_dir = fs::symlink_metadata(icons.join("default")).expect("dir exists");
        assert!(created_dir.is_dir() && !created_dir.file_type().is_symlink());

        snapshot.restore().expect("roll the creation back");
        assert!(!target.exists(), "the created file is removed");
        assert!(
            !icons.join("default").exists(),
            "the directories the call created are removed"
        );
        assert!(icons.is_dir(), "the pre-existing parent is kept");
        // Restoring twice is harmless: a file that is already gone counts as restored.
        snapshot.restore().expect("a second restore is a no-op");
    }

    #[test]
    fn create_atomic_refuses_an_existing_file_without_touching_it() {
        // Creation is exclusive: a file that exists (for instance one another
        // program created after the app looked) is never clobbered.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = dir.path().join("index.theme");
        fs::write(&target, b"theirs\n").expect("write the existing file");

        let result = create_atomic(&target, b"ours\n");
        assert!(
            matches!(result, Err(WriteError::AlreadyExists { .. })),
            "an existing target must be refused, got {result:?}"
        );
        assert_eq!(fs::read(&target).expect("read"), b"theirs\n");
        assert_eq!(
            fs::read_dir(dir.path()).expect("read dir").count(),
            1,
            "no temp file is left behind"
        );
    }

    #[test]
    fn create_atomic_never_replaces_a_dangling_symlink() {
        // A deployed link whose repo target is missing is still a link the user (or
        // setup.sh) made; replacing it with a plain file is exactly the breakage the
        // symlink-following rule exists to prevent.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let link = dir.path().join("index.theme");
        symlink(dir.path().join("missing-target"), &link).expect("create a dangling link");

        let result = create_atomic(&link, b"ours\n");
        assert!(
            matches!(result, Err(WriteError::AlreadyExists { .. })),
            "a dangling symlink must be refused, got {result:?}"
        );
        assert!(
            fs::symlink_metadata(&link)
                .expect("the link is still there")
                .file_type()
                .is_symlink()
        );
        assert!(!dir.path().join("missing-target").exists());
    }

    #[test]
    fn create_atomic_below_a_dangling_symlinked_parent_creates_nothing() {
        // `~/.icons` is a deployment link whose target is missing. The directory walk
        // stops at the link; creating `default/` below it follows the link and fails,
        // so the call is a Write error that creates nothing and keeps the link.
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("temp dir should be creatable");
        let icons = home.path().join(".icons");
        let missing = home.path().join("missing-icons-target");
        symlink(&missing, &icons).expect("create a dangling parent link");
        let target = icons.join("default").join("index.theme");

        let result = create_atomic(&target, b"[Icon Theme]\n");
        assert!(
            matches!(result, Err(WriteError::Write { .. })),
            "a dangling parent must fail the directory creation, got {result:?}"
        );
        assert!(
            fs::symlink_metadata(&icons)
                .expect("the link is still there")
                .file_type()
                .is_symlink(),
            "the dangling link is preserved"
        );
        assert!(!missing.exists(), "the link target is not created");
        let entries: Vec<_> = fs::read_dir(home.path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(".icons")]);
    }

    #[test]
    fn rollback_keeps_a_created_file_that_another_program_rewrote() {
        // The rollback of a creation deletes the file only while it holds the bytes
        // the app wrote. A file rewritten by someone else in between is theirs now.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = dir.path().join("default").join("index.theme");

        let snapshot = create_atomic(&target, b"ours\n").expect("create the file");
        fs::write(&target, b"theirs\n").expect("an external rewrite");

        snapshot.restore().expect("the rollback itself succeeds");
        assert_eq!(
            fs::read(&target).expect("the file survives"),
            b"theirs\n",
            "a file changed by another program is never deleted"
        );
        assert!(dir.path().join("default").is_dir());
    }

    #[test]
    fn create_atomic_follows_a_symlinked_parent_directory() {
        // A parent directory deployed as a symlink is followed like any other link:
        // the new file is created in the directory it points to.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let real = dir.path().join("real-default");
        fs::create_dir(&real).expect("create the real directory");
        let linked = dir.path().join("default");
        symlink(&real, &linked).expect("link the parent directory");

        let snapshot =
            create_atomic(&linked.join("index.theme"), b"x\n").expect("create through the link");
        assert_eq!(fs::read(real.join("index.theme")).expect("read"), b"x\n");
        assert_eq!(
            snapshot.resolved_path(),
            fs::canonicalize(&real)
                .expect("canonicalize")
                .join("index.theme")
        );
        assert!(
            fs::symlink_metadata(&linked)
                .expect("the link is kept")
                .file_type()
                .is_symlink()
        );

        snapshot.restore().expect("roll back");
        assert!(!real.join("index.theme").exists());
        assert!(real.is_dir(), "a directory the call did not create is kept");
    }
}
