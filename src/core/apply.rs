//! The Apply pipeline orchestrator: the fixed-order, transactional write +
//! reload machinery run when the user clicks **Apply** (task 4.5; architecture
//! §6; R5.3, R5.4, R5.5, R5.6, R8.3, R7.3, R6.2).
//!
//! # What this module is
//!
//! This is the capstone that ties the system boundary (§2) and the rest of the
//! core domain (§4) together. Given a page's [`ApplyPlan`] it runs the exact
//! ordered pipeline from architecture §6 and returns a structured
//! [`ApplyOutcome`] the UI (task 5.3) surfaces. It lives in `core/` — GTK-free,
//! headlessly testable (R6.2), enforced by `tests/module_boundaries.rs` — and
//! reaches every side effect only through the seams: file writes through
//! [`crate::system::writer`], subprocess reloads and the palette generator
//! through [`CommandRunner`], and the kitty/hypridle signals through
//! [`ProcessSignaller`]. No shell is ever invoked.
//!
//! # The fixed order (load-bearing)
//!
//! The ordering below is the contract, not an implementation detail — the app
//! must never break a working desktop (R8.3), so each step is a gate that aborts
//! before the next when it fails:
//!
//! 1. **Validate all** dirty staged values via [`SettingId::validate`] (R8.3).
//!    Any invalid value aborts *before any write* and the errors are returned;
//!    nothing is written. (The store already validates on stage, so this is a
//!    final guard — and it genuinely re-checks time-sensitive rules like a
//!    wallpaper path that may have been deleted since it was staged.)
//! 2. **Conflict check** (R5.6): re-read and hash the tracked files via
//!    [`FreshnessTracker::check_conflicts`], then split the result by whether the
//!    file is one this apply would write (see [`partition_conflicts`]):
//!    - a **write target** that changed externally since it was loaded aborts the
//!      apply *before any write*, so the external edit is surfaced rather than
//!      clobbered ([`ApplyOutcome::Conflicted`]);
//!    - any **other** tracked file that changed does not block the apply. It is
//!      logged and handed back as
//!      [`unrelated_conflicts`](ApplyOutcome::Applied::unrelated_conflicts) for the
//!      UI to mention, because the store's in-memory copy of that file is now stale.
//!
//!    Scoping the *blocking* to the write targets is what keeps the pipeline
//!    recoverable (task 9.11). Blocking on the whole tracker was safe but a dead
//!    end: the store can only clear a baseline for a file it can re-read and
//!    re-parse, so a tracked file that was deleted, had its permissions revoked, or
//!    was left unparseable by a hand edit stays in conflict for the rest of the
//!    session — and while it did, *every* category's apply returned `Conflicted`
//!    with no way out short of restarting the app. Narrowing it costs nothing in
//!    safety: R5.6 is about not clobbering an external edit, and an apply cannot
//!    clobber a file it does not write. The problem file's own category stays
//!    protected twice over — the conflict blocks the apply the moment that file
//!    becomes a write target, and each page's write glue re-reads its backing file
//!    to render the edit, so an unreadable one aborts the apply before the plan is
//!    even assembled. What the narrowing does give up is the incidental early
//!    warning the whole-tracker check gave for a file with no pending edits, which
//!    is exactly why those conflicts are still *returned* rather than dropped: the
//!    user is told and recovers with Refresh, which reloads and re-baselines what it
//!    can (R4.3).
//! 3. **Atomic writes with per-file rollback** (R5.4): write each planned file
//!    through [`write_atomic`], collecting a [`FileSnapshot`] per success. If any
//!    write fails, every already-written file is rolled back from its in-memory
//!    pre-apply snapshot and the failure is returned, leaving the desktop as it
//!    was.
//! 4. **Palette generator, last** (the palette gotcha): if the change is a
//!    color-scheme switch, run the discovered `scripts/generate-colors <scheme>`
//!    as the **last** write step. A non-zero exit — which also covers a
//!    missing/incomplete `theme/fonts`, since the generator aborts on it — is a
//!    write failure and triggers the same rollback of the earlier app-written
//!    files. Ordering it last is what guarantees a rollback never leaves the
//!    generated files (the six color partials, three font partials, and the
//!    marker) stranded on the new scheme: a failure anywhere in step 3 happens
//!    *before* the generator runs, so the generator never runs on a doomed apply.
//! 5. **Reloads** (R5.5): only for components whose backing file changed *and*
//!    which detection found running, via [`plan_reloads`] + `execute`, gated on
//!    [`Capabilities`]. Reload failures are **non-fatal** — the writes stand —
//!    but are logged at `error` and returned so the UI can toast them, and the
//!    remaining reloads are still attempted.
//!
//! # Logging (R7.3)
//!
//! Each successful write is logged with its path and the changed-key labels (never
//! the file contents); the palette generator, each reload, and every abort/error
//! are logged too. The lower-level byte counts and exit statuses are logged by the
//! writer and the [`CommandRunner`] themselves; this module layers the
//! apply-level record (which keys, which step) on top.
//!
//! # The seam: how a page produces an [`ApplyPlan`]
//!
//! The pipeline is deliberately **parser-agnostic** — it orchestrates, the pages
//! produce the bytes. Translating a `SettingId` + staged [`Value`] into concrete
//! new file bytes goes through the format parsers (§3) and is page-specific glue
//! (the §6 category tasks). So a page assembles an [`ApplyPlan`] like this:
//!
//! - **`validations`**: the dirty staged values to re-check — the caller reads
//!   them from the [`SettingsStore`](crate::core) (`dirty_ids()` mapped through
//!   `value()`).
//! - **`writes`**: one [`FileWrite`] per changed backing file, each carrying the
//!   file's live XDG path, the *complete* new bytes the page produced by applying
//!   its staged edits through the relevant parser (surgical, span-preserving —
//!   §3), the changed-key labels for logging, the [`BackingFile`] the file
//!   maps to (which drives its reload), and a [`WriteValidation`] declaring whether
//!   the write's values are among the `validations` above (read only by the
//!   plan-drift guard, [`warn_on_missing_validations`]).
//! - **`palette`**: `Some` only for a color-scheme switch, carrying the scheme
//!   name and the `scripts/generate-colors` path discovered from the capabilities
//!   palette source (R8.5). A v1 palette switch edits no file directly — it runs
//!   the generator, which regenerates the read-only color files — so it appears
//!   here rather than in `writes`.
//! - **`reload_params`**: the runtime values the parameterized reloads need
//!   (wallpaper path, cursor theme/size, GTK/icon theme names), each set only for
//!   a value that actually changed (task 4.4).
//!
//! The conflict-check step operates on the freshness baselines the store recorded
//! when it read those same files, so the caller passes the store's
//! [`FreshnessTracker`]. The set of changed [`BackingFile`]s that drives the
//! reload plan is derived here from the writes (plus a palette switch), so the
//! caller never has to keep two lists in sync.
//!
//! # Committing a successful apply (the caller's responsibility)
//!
//! This pipeline is a **pure orchestrator** — it never touches the
//! [`SettingsStore`](crate::core). So after an [`ApplyOutcome::Applied`] the caller
//! (task 5.3) **must** commit the apply to the store, via
//! [`SettingsStore::commit_apply`](crate::core::store::SettingsStore::commit_apply),
//! which does two things the pipeline cannot:
//!
//! - promotes every pending staged value to its `original` (the edits are now the
//!   on-disk truth, so the store is clean again);
//! - re-baselines the written files' freshness from the exact bytes just written.
//!
//! The second is not optional: the app just rewrote those files, so their on-disk
//! bytes no longer match the pre-apply freshness baseline. Without the re-baseline,
//! the **next** apply's step-2 conflict check would hash the app's own just-written
//! bytes against the stale baseline, see a mismatch, and abort the whole apply as
//! [`Conflicted`](ApplyOutcome::Conflicted) — a spurious self-conflict. Committing
//! after `Applied` closes that loop. It is safe to keep in the store/UI layer
//! (rather than the pipeline) precisely because the pipeline reports `written` so
//! the caller knows exactly which files to re-baseline.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use crate::core::detect::Capabilities;
use crate::core::freshness::{Conflict, FreshnessTracker};
use crate::core::model::{SettingId, ValidationError, Value};
use crate::core::reload::{BackingFile, ReloadError, ReloadParams, plan_reloads};
use crate::system::command::{Command, CommandError, CommandRunner, STDERR_EXCERPT_LIMIT};
use crate::system::signal::ProcessSignaller;
use crate::system::writer::{FileSnapshot, WriteError, write_atomic};

/// One planned atomic write of a backing config file — the pipeline's file-write
/// seam (see the module docs).
///
/// A page produces one of these per changed file by rendering its staged edits
/// through the relevant parser (§3). The pipeline treats the bytes as opaque: it
/// only writes them atomically and, on a later failure, rolls them back.
#[derive(Clone, Debug)]
pub struct FileWrite {
    /// The live XDG runtime path to rewrite (R8.5). [`write_atomic`] canonicalizes
    /// it, so a symlink into a dotfiles repo has its real target rewritten and the
    /// link preserved; a plain file is rewritten in place.
    pub path: PathBuf,
    /// The complete new file contents, produced by the page's parser glue from the
    /// staged values (surgical, span-preserving — §3). Written verbatim.
    pub contents: Vec<u8>,
    /// Human-readable labels of the keys/values this write changes, logged with the
    /// path at `info` on a successful write (R7.3). Never the file contents.
    pub changed_keys: Vec<String>,
    /// The reload concern this file drives, used to plan the post-write reloads
    /// (task 4.4).
    pub backing: BackingFile,
    /// Where the values in this write get their R8.3 validation — declared by whoever
    /// built the write, and read only by the plan-drift guard
    /// ([`warn_on_missing_validations`]). See [`WriteValidation`].
    pub validation: WriteValidation,
}

/// Whether a [`FileWrite`]'s values are covered by its plan's
/// [`validations`](ApplyPlan::validations) — the provenance signal the plan-drift guard
/// reads (task 9.10).
///
/// The pipeline cannot work this out for itself: a [`FileWrite`] is opaque bytes plus
/// labels, so nothing in it says which [`SettingId`] produced it. Only the code that
/// *built* the write knows, so it declares it here. The distinction is per **write**
/// rather than per file or per page because the same backing file can carry either kind
/// depending on which value changed — a `hyprpaper.conf` write carries a user-chosen
/// image path (validated: it must still exist and be readable) when the wallpaper
/// changed, but carries only a fit mode picked from a fixed list when the user changed
/// just the fit, and validating the *unchanged* wallpaper path in that second case could
/// abort an apply over a file the user never touched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteValidation {
    /// The plan's `validations` re-check the value(s) this write carries, so an empty
    /// `validations` list alongside this write means the caller dropped them (the drift
    /// the guard warns about).
    InPlan,
    /// This write carries nothing for step 1 to check, so its absence from `validations`
    /// is correct rather than a bug. Two cases exist today, both documented where they
    /// are declared: a change confined to values chosen from a list the app itself
    /// discovered or defined (the GTK/icon/cursor theme names and sizes — see task 9.27,
    /// which will give those validations of their own — and the wallpaper fit mode).
    NotNeeded,
}

/// A palette (color-scheme) switch — the last write step (the palette gotcha, §6
/// step 3).
///
/// A v1 palette switch does not edit any file directly: it runs
/// `scripts/generate-colors <scheme>`, which reads the (unchanged) `colors/<scheme>`
/// source and regenerates the read-only color/font partials and the marker. It is
/// therefore modelled separately from [`FileWrite`] and always run *after* every
/// file write, so a rollback never strands the generated files on the new scheme.
#[derive(Clone, Debug)]
pub struct PaletteSwitch {
    /// The scheme name passed to the generator (e.g. `nord`).
    pub scheme: String,
    /// The discovered `scripts/generate-colors` path (from the capabilities palette
    /// source, R8.5). Run as `generate-colors <scheme>` through the
    /// [`CommandRunner`] — no shell.
    pub generate_colors: PathBuf,
}

/// A complete, self-contained description of one Apply — the pipeline's input.
///
/// Assembled by the page (see the module docs on the seam). Held by reference by
/// [`run`], which only reads it.
///
/// `validations` and `writes` are two views of the *same* dirty settings and are
/// expected to be derived together from the store by the task-5.3 glue — the
/// validations from `dirty_ids()`/`value()`, the writes from rendering those same
/// staged values through the parsers. The two are independent fields (the pipeline
/// treats them separately), so a buggy caller could let them drift; the contract is
/// that every dirty setting appears in both. [`run`] guards the most damaging drift —
/// writes with no validations at all, which would let step 1's R8.3 gate pass
/// vacuously — by logging a `warn`; see [`warn_on_missing_validations`].
#[derive(Clone, Debug)]
pub struct ApplyPlan {
    /// The dirty staged values to re-validate before any write (step 1, R8.3),
    /// read by the caller from the store's dirty settings.
    pub validations: Vec<(SettingId, Value)>,
    /// The backing-file writes to apply atomically, in order (step 3, R5.4).
    pub writes: Vec<FileWrite>,
    /// The palette switch to run last, if this Apply changes the color scheme
    /// (step 4).
    pub palette: Option<PaletteSwitch>,
    /// The runtime values the parameterized reloads need (step 5, task 4.4).
    pub reload_params: ReloadParams,
}

/// A staged value that failed validation, with the setting it belongs to (R8.3).
#[derive(Debug)]
pub struct InvalidSetting {
    /// The setting whose staged value was rejected.
    pub id: SettingId,
    /// Why it was rejected — its [`Display`](std::fmt::Display) is UI-ready.
    pub error: ValidationError,
}

/// Why the write phase (steps 3–4) failed, after which the earlier writes were
/// rolled back.
#[derive(Debug)]
pub enum WriteFailureCause {
    /// A backing-file write failed (step 3). Carries the live path that was being
    /// written and the underlying writer error.
    File {
        /// The live path the failed write targeted.
        path: PathBuf,
        /// The underlying writer error.
        error: WriteError,
    },
    /// The palette generator ran but exited non-zero (step 4). This is the failure
    /// path that also covers a missing/incomplete `theme/fonts`, since
    /// `generate-colors` aborts on it. Carries the exit code, or `None` if it was
    /// terminated by a signal, plus what the generator wrote to standard error.
    GenerateColorsExit {
        /// The generator's exit code, or `None` on a signal termination.
        code: Option<i32>,
        /// A bounded, presentable excerpt of what the generator wrote to standard
        /// error, or `None` when it wrote nothing. This is the actionable half of
        /// the failure — the exit code alone says only *that* the palette could not
        /// be regenerated, whereas the generator's own message says *why* (e.g. that
        /// `theme/fonts` is missing or incomplete), so it is carried into the
        /// [`Display`](std::fmt::Display) message the UI shows. See
        /// [`stderr_excerpt`] for how it is bounded.
        stderr_excerpt: Option<String>,
    },
    /// The palette generator could not be run at all — a spawn failure (not on
    /// `PATH`) or a timeout (step 4).
    GenerateColorsUnrunnable(CommandError),
}

impl fmt::Display for WriteFailureCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteFailureCause::File { path, error } => {
                write!(f, "failed to write {}: {error}", path.display())
            }
            WriteFailureCause::GenerateColorsExit {
                code,
                stderr_excerpt,
            } => {
                match code {
                    Some(code) => write!(f, "generate-colors exited with status {code}")?,
                    None => f.write_str("generate-colors was terminated by a signal")?,
                }
                // Append the generator's own diagnostic when there is one: it is what
                // tells the user which of the generator's preconditions failed. The
                // excerpt is passed through verbatim (see `stderr_excerpt`), so the
                // message the user reads is the one the generator wrote.
                match stderr_excerpt {
                    Some(excerpt) => write!(f, ": {excerpt}"),
                    None => Ok(()),
                }
            }
            WriteFailureCause::GenerateColorsUnrunnable(error) => {
                write!(f, "generate-colors could not be run: {error}")
            }
        }
    }
}

/// The result of the write phase failing and the pipeline rolling back (R5.4).
///
/// The desktop is left as it was *unless* [`rollback_failures`](Self::rollback_failures)
/// is non-empty, in which case some earlier file could not be restored and the UI
/// must warn prominently.
///
/// Note the two path representations here differ, so a UI rendering both should say
/// so: [`WriteFailureCause::File`]'s `path` is the **live** path the failed write
/// targeted (a resolution failure may mean it has no resolved form at all), whereas
/// [`rolled_back`](Self::rolled_back) and [`rollback_failures`](Self::rollback_failures)
/// hold the **resolved** (symlink-followed) paths taken from the snapshots of files
/// that had already been written.
#[derive(Debug)]
pub struct WriteFailure {
    /// What went wrong in the write phase.
    pub cause: WriteFailureCause,
    /// The resolved (symlink-followed) paths of files that had already been written
    /// and were successfully restored to their pre-apply contents.
    pub rolled_back: Vec<PathBuf>,
    /// Rollback restores that themselves failed, each with the resolved path and the
    /// writer error. Non-empty means the desktop may be left partially changed.
    pub rollback_failures: Vec<(PathBuf, WriteError)>,
}

/// The terminal state of an Apply, for the UI (task 5.3) to surface (R5.3–R5.6).
///
/// Exactly one of these is returned. The first three are aborts that leave every
/// backing file untouched (nothing was written); the last means the writes stand.
/// The enum carries error types that are neither `Clone` nor `PartialEq`, so it
/// derives only [`Debug`]; callers match on the variant and render the messages.
#[derive(Debug)]
pub enum ApplyOutcome {
    /// Step 1 failed: one or more staged values are invalid (R8.3). Nothing was
    /// written; the UI shows the errors and keeps the staged edits for correction.
    ValidationFailed(Vec<InvalidSetting>),
    /// Step 2 failed: one or more files this apply would have **written** changed on
    /// disk since load (R5.6). Nothing was written; the UI warns and reloads rather
    /// than clobber.
    ///
    /// Carries only the conflicts that blocked the apply, i.e. the ones the user has
    /// to resolve before it can succeed. Conflicts in tracked files this apply does
    /// not write never block it (see the module docs on step 2) and are reported
    /// through [`Applied`](Self::Applied)'s `unrelated_conflicts` instead — on an
    /// aborted apply they are only logged, since the blocking ones are what the user
    /// must act on.
    Conflicted(Vec<Conflict>),
    /// Step 3 or 4 failed: a file write or the palette generator failed. Every
    /// already-written file was rolled back to its pre-apply snapshot (R5.4).
    WriteFailed(WriteFailure),
    /// Steps 1–4 succeeded: every file write stands. Any reload failures are
    /// non-fatal (R5.5) and listed here for the UI to toast; an empty list is a
    /// fully clean apply.
    ///
    /// After this outcome the caller **must commit the apply to the store** — see
    /// the module docs' "Committing a successful apply" section: the pipeline is a
    /// pure orchestrator and never touches the store, so the store's dirty state
    /// and freshness baselines are only reconciled by that commit.
    Applied {
        /// The reloads that failed after the writes succeeded, in the order they
        /// were attempted. The writes are not rolled back for these.
        reload_failures: Vec<ReloadError>,
        /// The **live** paths of the files that were written (every entry of the
        /// plan's `writes`, since an `Applied` outcome means they all succeeded).
        ///
        /// Live rather than resolved paths because the store keys its freshness
        /// baselines by the live path it loaded each file with; re-baselining after
        /// the commit must overwrite those same keys, so the caller needs the live
        /// paths (not the symlink-resolved targets the snapshots hold). Also lets
        /// the UI report exactly which files changed.
        written: Vec<PathBuf>,
        /// Tracked backing files that changed on disk since they were loaded but that
        /// this apply did **not** write, so they never blocked it (step 2, R5.6; task
        /// 9.11).
        ///
        /// Empty on the ordinary apply. A non-empty list means nothing was clobbered —
        /// these files were left exactly as the other program left them — but the
        /// store's in-memory originals for them are now stale, and one of them may no
        /// longer be readable at all. The UI surfaces them as a non-fatal note so the
        /// user knows to Refresh (which reloads and re-baselines what it can, R4.3)
        /// rather than being told nothing until the next apply that touches one of
        /// those files aborts.
        unrelated_conflicts: Vec<Conflict>,
    },
}

/// Runs the Apply pipeline for `plan` in the fixed order (architecture §6).
///
/// `freshness` is the store's [`FreshnessTracker`], carrying the baselines recorded
/// when the backing files were read, so step 2 can detect an external edit.
/// `capabilities` gates the reloads (step 5) to components that are present and
/// running. `runner` and `signaller` are the side-effect seams every subprocess and
/// signal goes through — a test injects recorders to assert the exact sequence.
///
/// Returns the single [`ApplyOutcome`] describing where the pipeline ended. See the
/// module docs for the step-by-step contract; the load-bearing guarantees are the
/// **order** (validate → conflict → write → generate-colors → reload) and the
/// **rollback** (any write-phase failure restores every already-written file).
pub fn run(
    plan: &ApplyPlan,
    freshness: &FreshnessTracker,
    capabilities: &Capabilities,
    runner: &dyn CommandRunner,
    signaller: &dyn ProcessSignaller,
) -> ApplyOutcome {
    // Caller-drift guard, run before the gate it protects: a plan carrying file writes
    // with nothing to validate would sail through step 1 vacuously (see `ApplyPlan`'s
    // docs on the two fields being derived from the same dirty settings).
    warn_on_missing_validations(plan);

    // --- Step 1: validate all staged values before touching anything (R8.3) ---
    let invalid = validate_all(&plan.validations);
    if !invalid.is_empty() {
        tracing::warn!(
            count = invalid.len(),
            "aborting apply: staged values failed validation; nothing was written (R8.3)"
        );
        return ApplyOutcome::ValidationFailed(invalid);
    }

    // --- Step 2: conflict check — re-read + hash the tracked files (R5.6) ---
    let (blocking, unrelated_conflicts) = partition_conflicts(plan, freshness.check_conflicts());
    if !blocking.is_empty() {
        tracing::warn!(
            count = blocking.len(),
            "aborting apply: target files changed on disk since load; not clobbering (R5.6)"
        );
        return ApplyOutcome::Conflicted(blocking);
    }
    if !unrelated_conflicts.is_empty() {
        // Not a reason to abort: this apply writes none of these files, so it cannot
        // clobber them (task 9.11). `check_conflicts` has already logged each one with
        // its path and reason; this records the pipeline's decision to proceed, so the
        // journal shows the conflict *and* that it did not block the apply (R7.3).
        tracing::info!(
            count = unrelated_conflicts.len(),
            "tracked files changed on disk but are not written by this apply; \
             continuing and reporting them"
        );
    }

    // --- Steps 3 & 4: atomic writes then the palette generator, with rollback ---
    if let Err(failure) = write_phase(plan, runner) {
        // A write-phase failure rolls back every file written so far, leaving the
        // desktop as it was (R5.4). Because the generator runs last, a file-write
        // failure never ran it, so the generated files are untouched (the palette
        // gotcha); a generator failure fails loudly without partial output and its
        // snapshots roll back the app-written files.
        tracing::error!(cause = %failure.cause, "apply write phase failed; rolling back (R5.4)");
        let (rolled_back, rollback_failures) = roll_back(&failure.written);
        return ApplyOutcome::WriteFailed(WriteFailure {
            cause: failure.cause,
            rolled_back,
            rollback_failures,
        });
    }

    // --- Step 5: reload the changed + running components (R5.5) ---
    let reload_failures = reload_phase(plan, capabilities, runner, signaller);

    // The write phase returned `Ok`, so every planned write succeeded: the written
    // files are exactly the plan's write targets. Report their live paths so the
    // caller can commit the apply to the store (promote staged→original and
    // re-baseline these files' freshness — see the module docs). The pipeline never
    // touches the store itself.
    let written: Vec<PathBuf> = plan.writes.iter().map(|write| write.path.clone()).collect();

    tracing::info!(
        files_written = written.len(),
        palette_regenerated = plan.palette.is_some(),
        reload_failures = reload_failures.len(),
        unrelated_conflicts = unrelated_conflicts.len(),
        "apply completed; file writes stand"
    );
    ApplyOutcome::Applied {
        reload_failures,
        written,
        unrelated_conflicts,
    }
}

/// Splits the tracker's conflicts into the ones that must abort this apply and the ones
/// that are only reported (step 2, R5.6; task 9.11).
///
/// Returns `(blocking, unrelated)`: a conflict is **blocking** when its file is one of
/// `plan`'s write targets, because writing it would clobber whatever changed on disk (or,
/// for an unreadable file, would write bytes rendered from contents that could not be
/// verified). Every other conflict is unrelated to this apply — it writes those files
/// not at all — so it is reported to the caller instead of aborting the apply. See the
/// module docs on step 2 for why the blocking set is scoped this way.
///
/// "Writes those files not at all" covers the palette step too, which mutates files that
/// are *not* in `plan.writes` (the generated colour and font partials, `swaync/colors.css`,
/// `state/active-scheme`). That is safe only because generated files are read-only inputs
/// this app never loads into the store, so they can never be tracked and can never turn up
/// as a conflict here. If a generated file ever did become store-loaded, this partition
/// would call a conflict in it unrelated while a palette apply rewrote it — so keep that
/// invariant, or teach this function about the generator's outputs.
///
/// Matching is by exact path equality, which is sound because both sides are the *same*
/// live XDG path: the tracker is keyed by the path the loader read each file from
/// (R8.5), and a [`FileWrite`] carries the path its page was built with from that same
/// loader. It also rests on those paths never aliasing: two different path strings that
/// resolve to one file would let a real write target be classed unrelated and written
/// anyway. The app's tracked set is fixed and disjoint, so no such alias exists — but note
/// the whole-tracker check this replaced was alias-proof by construction and this one is
/// not, so a future tracked file reachable by two spellings needs care here.
/// Nothing here canonicalizes, deliberately — a deleted or permission-revoked
/// file cannot be canonicalized at all, and that is precisely the case this comparison
/// has to get right.
fn partition_conflicts(
    plan: &ApplyPlan,
    conflicts: Vec<Conflict>,
) -> (Vec<Conflict>, Vec<Conflict>) {
    conflicts.into_iter().partition(|conflict| {
        plan.writes
            .iter()
            .any(|write| write.path.as_path() == conflict.path())
    })
}

/// Validates every `(id, value)` pair, collecting the ones that fail (step 1).
///
/// Returns an empty vector when all pass. Each failure is logged at `warn` with the
/// setting and the reason (R7.3) so an aborted apply is diagnosable from the journal.
fn validate_all(validations: &[(SettingId, Value)]) -> Vec<InvalidSetting> {
    let mut invalid = Vec::new();
    for (id, value) in validations {
        if let Err(error) = id.validate(value) {
            tracing::warn!(?id, %error, "staged value is invalid (R8.3)");
            invalid.push(InvalidSetting { id: *id, error });
        }
    }
    invalid
}

/// The writes in `plan` that claim their values are validated by it, when it validates
/// nothing at all — the [`ApplyPlan`] drift guard's predicate (R8.3).
///
/// Returns an empty vector for a consistent plan. Two things narrow it deliberately:
///
/// - Only a plan with *no* `validations` whatsoever is flagged. That is the drift the
///   pipeline can actually detect; it cannot tell which setting a given [`FileWrite`]'s
///   bytes came from, so a plan that validates some of its writes' values but not others
///   is accepted as-is.
/// - Only writes declaring [`WriteValidation::InPlan`] count. A write that declares
///   [`WriteValidation::NotNeeded`] is validation-free by design (a theme-name change, a
///   wallpaper fit-only change), so flagging it would make the warning fire on ordinary,
///   correct applies — the fastest way to make a diagnostic worth ignoring.
fn writes_without_validations(plan: &ApplyPlan) -> Vec<&FileWrite> {
    if !plan.validations.is_empty() {
        return Vec::new();
    }
    plan.writes
        .iter()
        .filter(|write| write.validation == WriteValidation::InPlan)
        .collect()
}

/// Logs a `warn` when `plan` has file writes but nothing to validate, so a drifted
/// caller cannot let step 1's R8.3 gate pass vacuously and unnoticed.
///
/// **Why a `warn` rather than a `debug_assert!`.** The condition catches a *developer*
/// wiring mistake, which is the usual argument for a debug assertion, but all three
/// consequences of asserting are worse here:
///
/// - a `debug_assert!` is compiled out of the release build the user runs, so the one
///   place a plan assembled at runtime could actually drift unseen is the one place it
///   would be silent;
/// - panicking mid-Apply is a bad response to a *suspicion*: the plan's writes may be
///   perfectly valid, and taking down the app on a heuristic is exactly the "never
///   break a working desktop" failure R8.3 is about;
/// - the pipeline's own tests deliberately build minimal plans (a write, no
///   validations) to exercise one step in isolation, so an assertion would fail a dozen
///   tests that document no defect.
///
/// A `warn` always fires, in every build, and lands in the journal beside the rest of
/// the apply record (R7.3) where the changed-key labels name the writes involved.
fn warn_on_missing_validations(plan: &ApplyPlan) {
    let unvalidated = writes_without_validations(plan);
    if unvalidated.is_empty() {
        return;
    }
    let keys: Vec<&str> = unvalidated
        .iter()
        .flat_map(|write| write.changed_keys.iter().map(String::as_str))
        .collect();
    tracing::warn!(
        writes = unvalidated.len(),
        keys = ?keys,
        "apply plan carries file writes but no values to validate, so the validation \
         gate passed vacuously; the caller that assembled this plan has most likely \
         dropped its validations (R8.3)"
    );
}

/// A write-phase failure paired with the snapshots taken before it, so the caller
/// can roll those back.
///
/// The write phase must return both the reason it failed *and* the files it had
/// already written, so this bundles them and [`run`] unpacks them for the rollback.
struct WritePhaseFailure {
    /// What went wrong.
    cause: WriteFailureCause,
    /// The snapshots of the files written before the failure, in write order
    /// (newest last). [`run`] rolls these back on any write-phase failure.
    written: Vec<FileSnapshot>,
}

/// Performs the atomic writes (step 3) then the palette generator (step 4).
///
/// On success there is nothing to return — the writes stand. On failure it returns
/// a [`WritePhaseFailure`] carrying the cause *and* the snapshots of the files
/// written so far, so [`run`] rolls them back; nothing is rolled back here, keeping
/// this a straight-line sequence of side effects whose only job is to attempt the
/// writes in order and report the first failure with enough state to undo it.
fn write_phase(plan: &ApplyPlan, runner: &dyn CommandRunner) -> Result<(), WritePhaseFailure> {
    let mut snapshots: Vec<FileSnapshot> = Vec::new();

    // Step 3: write each backing file atomically, snapshotting for rollback.
    for write in &plan.writes {
        match write_atomic(&write.path, &write.contents) {
            Ok(snapshot) => {
                // R7.3: the apply-level write record — path + changed keys, never
                // the contents (the writer logs the resolved path + byte count).
                tracing::info!(
                    path = %write.path.display(),
                    keys = ?write.changed_keys,
                    "wrote backing file"
                );
                snapshots.push(snapshot);
            }
            Err(error) => {
                return Err(WritePhaseFailure {
                    cause: WriteFailureCause::File {
                        path: write.path.clone(),
                        error,
                    },
                    written: snapshots,
                });
            }
        }
    }

    // Step 4: the palette generator, LAST among write steps (the palette gotcha).
    if let Some(palette) = &plan.palette {
        let command =
            Command::new(palette.generate_colors.to_string_lossy()).arg(palette.scheme.as_str());
        match runner.run(&command) {
            Ok(output) if output.success() => {
                tracing::info!(scheme = %palette.scheme, "regenerated palette via generate-colors");
            }
            Ok(output) => {
                // Carry the generator's stderr, not just its exit code: it is the only
                // thing that says *why* the palette could not be regenerated, and the
                // runner captures it in full for exactly this caller.
                return Err(WritePhaseFailure {
                    cause: WriteFailureCause::GenerateColorsExit {
                        code: output.code(),
                        stderr_excerpt: stderr_excerpt(output.stderr()),
                    },
                    written: snapshots,
                });
            }
            Err(error) => {
                return Err(WritePhaseFailure {
                    cause: WriteFailureCause::GenerateColorsUnrunnable(error),
                    written: snapshots,
                });
            }
        }
    }

    Ok(())
}

/// Turns raw captured stderr into the bounded, presentable excerpt
/// [`WriteFailureCause::GenerateColorsExit`] carries, or `None` when the command wrote
/// nothing worth showing.
///
/// The four decisions behind the shape, since a user reads the result in a warning
/// dialog:
///
/// - **Bounded first, decoded second.** The leading [`STDERR_EXCERPT_LIMIT`] bytes are
///   taken and only then decoded, so the bound holds whatever the encoding; a
///   multi-byte character cut in half by the boundary simply decodes to the U+FFFD
///   replacement character.
/// - **Lossy decoding.** A script's diagnostic is not guaranteed to be UTF-8 (a
///   filename in another encoding is enough), and losing the whole message over one
///   stray byte would defeat the point, so invalid bytes become U+FFFD rather than an
///   error.
/// - **Trailing whitespace trimmed, and an ellipsis only when something was really
///   lost.** Practically every diagnostic ends in a newline, which would otherwise render
///   as a trailing blank line; the appended `…` tells the reader there was more output
///   than is shown. The marker is suppressed when everything past the cut is whitespace,
///   so a message of exactly the limit plus its trailing newline is not advertised as
///   truncated — it is complete, matching the trim applied to the kept text.
/// - **Otherwise verbatim.** The text is not re-wrapped, re-cased, re-ordered, or
///   interpreted — it is the generator's own message about its own failure, which is
///   what makes it actionable. Note this stays within R7.3: what must never be echoed
///   is *config file contents*, and a tool's error output about itself is not that.
fn stderr_excerpt(stderr: &[u8]) -> Option<String> {
    let end = stderr.len().min(STDERR_EXCERPT_LIMIT);
    let mut excerpt = String::from_utf8_lossy(&stderr[..end])
        .trim_end()
        .to_string();
    if excerpt.is_empty() {
        return None;
    }
    // Only whitespace beyond the cut means nothing the reader would have wanted was
    // dropped (`is_ascii_whitespace` covers the newlines, tabs and spaces a diagnostic
    // ends with; any other byte counts as content, so the marker errs towards honesty).
    let lost_content = stderr[end..].iter().any(|byte| !byte.is_ascii_whitespace());
    if lost_content {
        excerpt.push('…');
    }
    Some(excerpt)
}

/// Restores each snapshot, reporting which files were rolled back and which restores
/// failed (R5.4).
///
/// Snapshots are unwound newest-first, the conventional order for undoing a
/// sequence of writes (each restore is independent, so order does not affect
/// correctness). A restore that itself fails is logged at `error` and collected
/// rather than aborting the remaining rollbacks — one un-restorable file must not
/// leave the others un-rolled-back.
fn roll_back(snapshots: &[FileSnapshot]) -> (Vec<PathBuf>, Vec<(PathBuf, WriteError)>) {
    let mut rolled_back = Vec::new();
    let mut failures = Vec::new();
    for snapshot in snapshots.iter().rev() {
        let path = snapshot.resolved_path().to_path_buf();
        match snapshot.restore() {
            Ok(()) => rolled_back.push(path),
            Err(error) => {
                tracing::error!(
                    path = %path.display(),
                    %error,
                    "failed to roll back a written file; the desktop may be partially changed"
                );
                failures.push((path, error));
            }
        }
    }
    (rolled_back, failures)
}

/// Plans and runs the reloads for the changed components (step 5, R5.5).
///
/// The changed [`BackingFile`] set is derived from the writes plus a palette switch,
/// deduplicated, then turned into an ordered, capability-gated action list by
/// [`plan_reloads`] — so only components that changed *and* are running get a
/// reload. Each action is run through the seams; a failure is logged at `error`,
/// collected, and does **not** stop the remaining reloads or roll back any write
/// (R5.5).
fn reload_phase(
    plan: &ApplyPlan,
    capabilities: &Capabilities,
    runner: &dyn CommandRunner,
    signaller: &dyn ProcessSignaller,
) -> Vec<ReloadError> {
    // Derive the changed backing files (a set dedups the several files that share a
    // reload, e.g. the cursor's settings.ini/uwsm/hyprland.conf copies).
    let mut changed: BTreeSet<BackingFile> =
        plan.writes.iter().map(|write| write.backing).collect();
    if plan.palette.is_some() {
        changed.insert(BackingFile::Palette);
    }
    let changed: Vec<BackingFile> = changed.into_iter().collect();

    let actions = plan_reloads(&changed, &plan.reload_params, capabilities);

    let mut failures = Vec::new();
    for action in &actions {
        if let Err(error) = action.execute(runner, signaller) {
            // Non-fatal: the write stands. The action executor already logged the
            // detail; record it here so the pipeline can surface it as a toast and
            // still attempt the remaining reloads.
            tracing::error!(%error, "reload failed but the file write stands (R5.5)");
            failures.push(error);
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::path::Path;

    use nix::sys::signal::Signal;

    use crate::core::detect::{Binary, Daemon};
    use crate::core::freshness::ConflictReason;
    use crate::system::command::{CommandOutput, MockCommandRunner};
    use crate::system::signal::{MockProcessSignaller, SignalCall};

    /// Writes `contents` to a fresh file in `dir` and returns its path.
    fn write_file(dir: &tempfile::TempDir, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, contents).expect("test file should be writable");
        path
    }

    /// A freshness tracker baselined against the current on-disk bytes of each path,
    /// so the conflict check passes for an unmodified file.
    fn record_baseline(paths: &[&Path]) -> FreshnessTracker {
        let mut tracker = FreshnessTracker::new();
        for path in paths {
            tracker.record(*path).expect("record a freshness baseline");
        }
        tracker
    }

    /// Capabilities with `hyprctl` + a live Hyprland IPC socket and nothing else —
    /// enough for a plain `hyprctl reload`.
    fn hyprland_only() -> Capabilities {
        Capabilities::for_tests(&[Binary::Hyprctl], &[], true)
    }

    /// Capabilities with everything a palette reload chain needs live.
    fn palette_capabilities() -> Capabilities {
        Capabilities::for_tests(
            &[Binary::Hyprctl],
            &[Daemon::Eww, Daemon::Swaync, Daemon::Kitty],
            true,
        )
    }

    // --- (a) a display-class change: exact writes + `hyprctl reload`, nothing else -

    #[test]
    fn a_display_change_writes_the_file_and_runs_only_hyprctl_reload() {
        // Accept criterion (a): a display-class change writes exactly its file and
        // issues exactly `hyprctl reload`, with no other command and no signal.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let monitors = write_file(&dir, "monitors.conf", b"monitor=eDP-1,preferred,auto,1\n");
        let tracker = record_baseline(&[&monitors]);

        let plan = ApplyPlan {
            validations: vec![(SettingId::MonitorScale, Value::Float(1.25))],
            writes: vec![FileWrite {
                path: monitors.clone(),
                contents: b"monitor=eDP-1,preferred,auto,1.25\n".to_vec(),
                changed_keys: vec!["monitor:eDP-1 scale".to_string()],
                backing: BackingFile::MonitorsConf,
                validation: WriteValidation::InPlan,
            }],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = run(&plan, &tracker, &hyprland_only(), &runner, &signaller);

        match outcome {
            ApplyOutcome::Applied {
                reload_failures,
                written,
                unrelated_conflicts,
            } => {
                assert!(reload_failures.is_empty());
                assert_eq!(
                    written,
                    vec![monitors.clone()],
                    "the written live path is reported for the caller to commit"
                );
                assert!(
                    unrelated_conflicts.is_empty(),
                    "an untouched tracker reports nothing to warn about"
                );
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert_eq!(
            fs::read(&monitors).expect("read the rewritten file"),
            b"monitor=eDP-1,preferred,auto,1.25\n",
            "the display change must be written to disk"
        );
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").arg("reload")],
            "a display change reloads only via `hyprctl reload`, nothing else"
        );
        assert!(
            signaller.calls().is_empty(),
            "a display change signals no process"
        );
    }

    // --- (b) a palette change: generate-colors, then the reload chain ------------

    #[test]
    fn a_palette_switch_runs_generate_colors_then_the_reload_chain() {
        // Accept criterion (b): a palette change runs `generate-colors <scheme>` as
        // the last write step, then the apply-theme reload chain follows.
        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: Vec::new(),
            palette: Some(PaletteSwitch {
                scheme: "nord".to_string(),
                generate_colors: PathBuf::from("/fake/repo/scripts/generate-colors"),
            }),
            reload_params: ReloadParams::default(),
        };
        // A palette switch writes no tracked file, so an empty tracker is correct.
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![4242])]);

        let outcome = run(
            &plan,
            &tracker,
            &palette_capabilities(),
            &runner,
            &signaller,
        );

        match outcome {
            ApplyOutcome::Applied {
                reload_failures,
                written,
                ..
            } => {
                assert!(reload_failures.is_empty());
                assert!(
                    written.is_empty(),
                    "a palette switch writes no backing file directly"
                );
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        // generate-colors is recorded FIRST (it is the last *write* step), and the
        // reload chain — hyprctl reload, eww reload, swaync -rs — follows in order.
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("/fake/repo/scripts/generate-colors").arg("nord"),
                Command::new("hyprctl").arg("reload"),
                Command::new("eww").arg("reload"),
                Command::new("swaync-client").arg("-rs"),
            ]
        );
        // kitty is reloaded by a SIGUSR1 signal, not a subprocess.
        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: "kitty".to_string(),
                signal: Signal::SIGUSR1,
                pids: vec![4242],
            }]
        );
    }

    // --- (c) a validation failure: nothing written -------------------------------

    #[test]
    fn a_validation_failure_aborts_before_any_write() {
        // Accept criterion (c): an invalid staged value aborts at step 1, so no file
        // is written and no command runs.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = write_file(&dir, "hyprpaper.conf", b"preload = /old.png\n");
        let tracker = record_baseline(&[&target]);

        let plan = ApplyPlan {
            // A wallpaper path that does not exist fails validation (R8.3).
            validations: vec![(
                SettingId::WallpaperPath,
                Value::String("/definitely/missing.png".to_string()),
            )],
            writes: vec![FileWrite {
                path: target.clone(),
                contents: b"preload = /new.png\n".to_vec(),
                changed_keys: vec!["preload".to_string()],
                backing: BackingFile::HyprpaperConf,
                validation: WriteValidation::InPlan,
            }],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = run(&plan, &tracker, &caps, &runner, &signaller);

        match outcome {
            ApplyOutcome::ValidationFailed(invalid) => {
                assert_eq!(invalid.len(), 1);
                assert_eq!(invalid[0].id, SettingId::WallpaperPath);
                assert!(
                    !invalid[0].error.to_string().is_empty(),
                    "the validation error carries a UI-ready message"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
        assert_eq!(
            fs::read(&target).expect("read the untouched file"),
            b"preload = /old.png\n",
            "a validation failure must leave the file untouched"
        );
        assert!(
            runner.recorded().is_empty(),
            "nothing runs on a validation failure"
        );
        assert!(signaller.calls().is_empty());
    }

    // --- (d) an external conflict: nothing written -------------------------------

    #[test]
    fn an_external_conflict_aborts_before_any_write() {
        // Accept criterion (d): a target file changed on disk since load aborts at
        // step 2, so the staged write never clobbers the external edit (R5.6). The
        // conflicting file is this plan's write target, which is exactly what makes it
        // blocking (task 9.11 — see the module docs on step 2's scoping).
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let target = write_file(&dir, "input.conf", b"kb_layout = us\n");
        let mut tracker = FreshnessTracker::new();
        tracker.record(&target).expect("baseline the file");
        // Someone edits the file by hand after the baseline was taken.
        fs::write(&target, b"kb_layout = se\n").expect("external edit");

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: target.clone(),
                contents: b"kb_layout = us,se\n".to_vec(),
                changed_keys: vec!["kb_layout".to_string()],
                backing: BackingFile::InputConf,
                validation: WriteValidation::InPlan,
            }],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = run(&plan, &tracker, &hyprland_only(), &runner, &signaller);

        match outcome {
            ApplyOutcome::Conflicted(conflicts) => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].path(), target.as_path());
            }
            other => panic!("expected Conflicted, got {other:?}"),
        }
        assert_eq!(
            fs::read(&target).expect("read the file"),
            b"kb_layout = se\n",
            "the external edit stands; the staged write must not have clobbered it"
        );
        assert!(runner.recorded().is_empty(), "nothing runs on a conflict");
    }

    // --- step 2's scoping: only a write target's conflict blocks (task 9.11) ------

    /// Two tracked files in a fresh temp dir, returning `(dir, input_conf, hypridle_conf)`
    /// with both baselined in the returned tracker.
    ///
    /// The scoping tests all need the same two-file shape: the store tracks every backing
    /// file it loaded (`input.conf`, swaync's `config.json`, `hypridle.conf`), while any
    /// single apply writes only the files whose category the user actually edited. Which of
    /// the two a given test makes its write target is the test's choice — deliberately not
    /// baked into these names, since the blocking and non-blocking tests swap the roles.
    fn two_tracked_files() -> (tempfile::TempDir, PathBuf, PathBuf, FreshnessTracker) {
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let input_conf = write_file(&dir, "input.conf", b"kb_layout = us\n");
        let hypridle_conf = write_file(&dir, "hypridle.conf", b"listener { timeout = 300 }\n");
        let tracker = record_baseline(&[&input_conf, &hypridle_conf]);
        (dir, input_conf, hypridle_conf, tracker)
    }

    /// The `input.conf` write the scoping tests plan, given the path from
    /// [`two_tracked_files`].
    fn input_conf_write(path: &Path) -> FileWrite {
        FileWrite {
            path: path.to_path_buf(),
            contents: b"kb_layout = us,se\n".to_vec(),
            changed_keys: vec!["kb_layout".to_string()],
            backing: BackingFile::InputConf,
            validation: WriteValidation::InPlan,
        }
    }

    #[test]
    fn a_deleted_tracked_file_does_not_block_an_apply_to_other_files() {
        // Task 9.11, the defect this scoping fixes: a tracked backing file that was
        // deleted can never be re-baselined by the store's refresh, so a whole-tracker
        // conflict check turned it into a permanent block on *every* category's apply
        // until the app was restarted. The apply below writes only `input.conf`, so the
        // deleted `hypridle.conf` cannot be clobbered by it and must not stop it — but it
        // is still reported, because the app's view of that file is now wrong.
        let (_dir, written, other, tracker) = two_tracked_files();
        fs::remove_file(&other).expect("delete the other tracked file");

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![input_conf_write(&written)],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = run(&plan, &tracker, &hyprland_only(), &runner, &signaller);

        match outcome {
            ApplyOutcome::Applied {
                written: written_paths,
                unrelated_conflicts,
                ..
            } => {
                assert_eq!(written_paths, vec![written.clone()]);
                assert_eq!(
                    unrelated_conflicts.len(),
                    1,
                    "the deleted file is reported rather than dropped"
                );
                assert_eq!(unrelated_conflicts[0].path(), other.as_path());
                assert!(
                    matches!(
                        unrelated_conflicts[0].reason(),
                        ConflictReason::Unreadable(_)
                    ),
                    "a deleted file's conflict reason is Unreadable, got {:?}",
                    unrelated_conflicts[0].reason()
                );
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert_eq!(
            fs::read(&written).expect("read the rewritten file"),
            b"kb_layout = us,se\n",
            "the unrelated file's problem must not cost the user their applied edit"
        );
        assert!(
            !other.exists(),
            "the apply must not recreate the file it did not plan to write"
        );
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").arg("reload")],
            "the apply completes normally, reloads included"
        );
    }

    #[test]
    fn an_external_edit_to_a_file_this_apply_does_not_write_does_not_block_it() {
        // The same scoping for the recoverable reason (task 9.11): a file someone else
        // edited that this apply does not write is reported, not blocking. Its external
        // bytes must stand untouched — the apply writes only its own target — and the
        // user recovers the app's stale view of it with Refresh.
        let (_dir, written, other, tracker) = two_tracked_files();
        fs::write(&other, b"listener { timeout = 900 }\n").expect("external edit");

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![input_conf_write(&written)],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = run(&plan, &tracker, &hyprland_only(), &runner, &signaller);

        match outcome {
            ApplyOutcome::Applied {
                unrelated_conflicts,
                ..
            } => {
                assert_eq!(unrelated_conflicts.len(), 1);
                assert_eq!(unrelated_conflicts[0].path(), other.as_path());
                assert!(matches!(
                    unrelated_conflicts[0].reason(),
                    ConflictReason::ContentChanged
                ));
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert_eq!(
            fs::read(&other).expect("read the externally edited file"),
            b"listener { timeout = 900 }\n",
            "the external edit stands: this apply never targeted that file"
        );
    }

    #[test]
    fn a_deleted_write_target_still_aborts_the_whole_apply() {
        // The other half of the scoping: the file's *own* category is still protected.
        // Here the deleted file is the one the plan writes, so step 2 blocks — the apply
        // must never write bytes rendered from contents it could not verify. Nothing else
        // in the plan is written either: an apply is all-or-nothing (R5.4).
        let (_dir, other, target, tracker) = two_tracked_files();
        fs::remove_file(&target).expect("delete the write target");

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: target.clone(),
                contents: b"listener { timeout = 600 }\n".to_vec(),
                changed_keys: vec!["listener[1].timeout".to_string()],
                backing: BackingFile::HypridleConf,
                validation: WriteValidation::InPlan,
            }],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = run(&plan, &tracker, &hyprland_only(), &runner, &signaller);

        match outcome {
            ApplyOutcome::Conflicted(conflicts) => {
                assert_eq!(conflicts.len(), 1, "only the write target blocks");
                assert_eq!(conflicts[0].path(), target.as_path());
                assert!(matches!(
                    conflicts[0].reason(),
                    ConflictReason::Unreadable(_)
                ));
            }
            other => panic!("expected Conflicted, got {other:?}"),
        }
        assert!(
            !target.exists(),
            "a blocked apply must not create the missing target"
        );
        assert_eq!(
            fs::read(&other).expect("read the untouched file"),
            b"kb_layout = us\n",
            "the apply aborted before any write"
        );
        assert!(runner.recorded().is_empty(), "nothing runs on a conflict");
    }

    #[cfg(unix)]
    #[test]
    fn a_write_target_whose_contents_cannot_be_read_aborts_the_apply() {
        // The case where step 2 is the *only* thing standing between the user and a
        // clobber: a target that still exists (so the atomic writer would happily
        // rewrite it) but whose read permission was revoked after it was loaded, meaning
        // the app cannot know what is in it now. The write must not proceed.
        //
        // Guarded so it degrades to a no-op when the suite happens to run as root, where
        // the mode is ignored and the read still succeeds — the same guard the freshness
        // tests use.
        use std::os::unix::fs::PermissionsExt;

        let (_dir, _other, target, tracker) = two_tracked_files();
        let original = fs::read(&target).expect("read the target before locking it");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o000))
            .expect("revoke read permission");
        let unreadable = fs::read(&target).is_err();

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: target.clone(),
                contents: b"listener { timeout = 600 }\n".to_vec(),
                changed_keys: vec!["listener[1].timeout".to_string()],
                backing: BackingFile::HypridleConf,
                validation: WriteValidation::InPlan,
            }],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = run(&plan, &tracker, &hyprland_only(), &runner, &signaller);

        if unreadable {
            assert!(
                matches!(outcome, ApplyOutcome::Conflicted(_)),
                "an unverifiable write target must abort the apply, got {outcome:?}"
            );
        }
        // Restore the mode so the contents can be checked and the temp dir cleaned up.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644))
            .expect("restore permissions");
        if unreadable {
            assert_eq!(
                fs::read(&target).expect("read the target back"),
                original,
                "the file the app could not read must keep its bytes"
            );
        }
    }

    // --- (e) a mid-write failure: earlier files rolled back, no reloads ----------

    #[test]
    fn a_mid_write_failure_rolls_back_earlier_files_and_runs_no_reloads() {
        // Accept criterion (e): when a later write fails, earlier written files are
        // restored to their pre-apply bytes and no reload is attempted (R5.4).
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let first = write_file(&dir, "input.conf", b"original input\n");
        // The second write targets a path that does not exist, so `write_atomic`
        // fails at canonicalization — an injected mid-sequence write failure.
        let missing = dir.path().join("monitors.conf");
        let tracker = record_baseline(&[&first]);

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![
                FileWrite {
                    path: first.clone(),
                    contents: b"new input\n".to_vec(),
                    changed_keys: vec!["kb_layout".to_string()],
                    backing: BackingFile::InputConf,
                    validation: WriteValidation::InPlan,
                },
                FileWrite {
                    path: missing.clone(),
                    contents: b"new monitors\n".to_vec(),
                    changed_keys: vec!["monitor".to_string()],
                    backing: BackingFile::MonitorsConf,
                    validation: WriteValidation::InPlan,
                },
            ],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let outcome = run(&plan, &tracker, &hyprland_only(), &runner, &signaller);

        match outcome {
            ApplyOutcome::WriteFailed(failure) => {
                match &failure.cause {
                    WriteFailureCause::File { path, .. } => assert_eq!(path, &missing),
                    other => panic!("expected a File write failure, got {other:?}"),
                }
                assert_eq!(
                    failure.rolled_back,
                    vec![fs::canonicalize(&first).expect("canonicalize the first file")],
                    "the successfully-written first file must be rolled back"
                );
                assert!(failure.rollback_failures.is_empty());
            }
            other => panic!("expected WriteFailed, got {other:?}"),
        }
        assert_eq!(
            fs::read(&first).expect("read the rolled-back file"),
            b"original input\n",
            "the earlier write must be rolled back to its original bytes"
        );
        assert!(
            runner.recorded().is_empty(),
            "a write-phase failure must run no reloads"
        );
        assert!(signaller.calls().is_empty());
    }

    // --- (f) a generate-colors failure: earlier files rolled back ----------------

    #[test]
    fn a_generate_colors_non_zero_exit_rolls_back_earlier_writes() {
        // Accept criterion (f): a non-zero `generate-colors` exit is a write failure
        // that rolls back the earlier file writes. This also proves the ordering —
        // the file write happened (it is rolled back) *before* generate-colors ran,
        // and generate-colors is the last write step (no reloads follow its failure).
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let monitors = write_file(&dir, "monitors.conf", b"monitor=eDP-1,preferred,auto,1\n");
        let tracker = record_baseline(&[&monitors]);

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: monitors.clone(),
                contents: b"monitor=eDP-1,preferred,auto,1.25\n".to_vec(),
                changed_keys: vec!["monitor:eDP-1 scale".to_string()],
                backing: BackingFile::MonitorsConf,
                validation: WriteValidation::InPlan,
            }],
            palette: Some(PaletteSwitch {
                scheme: "nord".to_string(),
                generate_colors: PathBuf::from("/fake/repo/scripts/generate-colors"),
            }),
            reload_params: ReloadParams::default(),
        };
        // generate-colors exits non-zero (also the missing/incomplete theme/fonts case).
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![1])]);

        let outcome = run(
            &plan,
            &tracker,
            &palette_capabilities(),
            &runner,
            &signaller,
        );

        match outcome {
            ApplyOutcome::WriteFailed(failure) => {
                assert!(
                    matches!(
                        failure.cause,
                        // This generator printed nothing, so there is no excerpt to
                        // carry — the message must then be the bare exit-code one,
                        // with no dangling separator (asserted below).
                        WriteFailureCause::GenerateColorsExit {
                            code: Some(1),
                            stderr_excerpt: None
                        }
                    ),
                    "the cause must be a non-zero generate-colors exit, got {:?}",
                    failure.cause
                );
                assert_eq!(
                    failure.cause.to_string(),
                    "generate-colors exited with status 1"
                );
                assert_eq!(
                    failure.rolled_back,
                    vec![fs::canonicalize(&monitors).expect("canonicalize")],
                    "the file written before generate-colors must be rolled back"
                );
            }
            other => panic!("expected WriteFailed, got {other:?}"),
        }
        assert_eq!(
            fs::read(&monitors).expect("read the rolled-back file"),
            b"monitor=eDP-1,preferred,auto,1\n",
            "the earlier write must be rolled back when generate-colors fails"
        );
        // Only generate-colors ran — the reload chain is never reached, so a failed
        // apply never leaves the generated files on the new scheme.
        assert_eq!(
            runner.recorded(),
            vec![Command::new("/fake/repo/scripts/generate-colors").arg("nord")],
        );
        assert!(
            signaller.calls().is_empty(),
            "no reload signals when the write phase fails"
        );
    }

    #[test]
    fn a_generate_colors_failure_carries_the_generators_stderr_into_its_message() {
        // Task 9.10: the exit code alone does not say *why* the palette could not be
        // regenerated. Inject the documented `theme/fonts` abort — the generator exits
        // non-zero after printing the reason — and assert the reason survives into the
        // rendered failure message, which is what the user is shown.
        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: Vec::new(),
            palette: Some(PaletteSwitch {
                scheme: "nord".to_string(),
                generate_colors: PathBuf::from("/fake/repo/scripts/generate-colors"),
            }),
            reload_params: ReloadParams::default(),
        };
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            1,
            "",
            "generate-colors: theme/fonts is missing or incomplete; aborting\n",
        ))]);
        let signaller = MockProcessSignaller::new();

        let outcome = run(
            &plan,
            &tracker,
            &palette_capabilities(),
            &runner,
            &signaller,
        );

        match outcome {
            ApplyOutcome::WriteFailed(failure) => {
                assert_eq!(
                    failure.cause.to_string(),
                    "generate-colors exited with status 1: generate-colors: theme/fonts is \
                     missing or incomplete; aborting",
                    "the rendered message must carry the generator's own diagnostic, \
                     trailing newline trimmed"
                );
            }
            other => panic!("expected WriteFailed, got {other:?}"),
        }
    }

    #[test]
    fn the_stderr_excerpt_is_byte_bounded_lossy_and_trimmed() {
        // The excerpt is what a user reads in a warning dialog, so each bound matters:
        // it must not grow without limit, must survive non-UTF-8 output, and must not
        // end in the newline practically every diagnostic carries.
        assert_eq!(
            stderr_excerpt(b"missing theme/fonts\n"),
            Some("missing theme/fonts".to_string()),
            "the trailing newline is trimmed"
        );
        assert_eq!(
            stderr_excerpt(b""),
            None,
            "a silent generator carries no excerpt"
        );
        assert_eq!(
            stderr_excerpt(b"\n \n"),
            None,
            "whitespace-only output carries no excerpt either, so the message cannot \
             end in a dangling separator"
        );

        // Exactly the limit plus its trailing newline: the only thing past the cut is
        // whitespace, which the trim would have removed anyway, so the excerpt is
        // complete and must not be advertised as truncated.
        let mut exact = vec![b'z'; STDERR_EXCERPT_LIMIT];
        exact.push(b'\n');
        assert_eq!(
            stderr_excerpt(&exact),
            Some("z".repeat(STDERR_EXCERPT_LIMIT)),
            "cutting only trailing whitespace is not a truncation"
        );

        // Over the limit: bounded to STDERR_EXCERPT_LIMIT bytes plus the one-character
        // truncation marker, never the whole flood.
        let flood = vec![b'x'; STDERR_EXCERPT_LIMIT * 3];
        let excerpt = stderr_excerpt(&flood).expect("a chatty generator carries an excerpt");
        assert_eq!(
            excerpt,
            format!("{}…", "x".repeat(STDERR_EXCERPT_LIMIT)),
            "the excerpt is capped and marked as truncated"
        );

        // Invalid UTF-8 must not cost the user the message: the bad byte is replaced.
        let mut invalid = b"bad byte: ".to_vec();
        invalid.push(0xff);
        assert_eq!(
            stderr_excerpt(&invalid),
            Some("bad byte: \u{fffd}".to_string()),
            "non-UTF-8 output decodes lossily rather than being dropped"
        );

        // A multi-byte character split by the byte boundary must not panic, and the
        // bound must still hold (the split character decodes to one replacement char).
        let mut split = vec![b'y'; STDERR_EXCERPT_LIMIT - 1];
        split.extend_from_slice("é".as_bytes());
        let excerpt = stderr_excerpt(&split).expect("an excerpt is produced");
        assert_eq!(
            excerpt,
            format!("{}\u{fffd}…", "y".repeat(STDERR_EXCERPT_LIMIT - 1)),
            "a character cut in half by the byte bound becomes a replacement char"
        );
    }

    #[test]
    fn a_generate_colors_spawn_failure_rolls_back_and_surfaces_a_command_error() {
        // A `generate-colors` that cannot be run at all (not on PATH) is likewise a
        // write failure that rolls back the earlier writes, surfaced distinctly so
        // the UI can tell "the generator is missing" from "the generator failed".
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let monitors = write_file(&dir, "monitors.conf", b"monitor=eDP-1,preferred,auto,1\n");
        let tracker = record_baseline(&[&monitors]);

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: monitors.clone(),
                contents: b"monitor=eDP-1,preferred,auto,1.25\n".to_vec(),
                changed_keys: vec!["monitor:eDP-1 scale".to_string()],
                backing: BackingFile::MonitorsConf,
                validation: WriteValidation::InPlan,
            }],
            palette: Some(PaletteSwitch {
                scheme: "nord".to_string(),
                generate_colors: PathBuf::from("/fake/generate-colors"),
            }),
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::with_outcomes([Err(CommandError::Spawn(io::Error::from(
            io::ErrorKind::NotFound,
        )))]);
        let signaller = MockProcessSignaller::new();

        let outcome = run(
            &plan,
            &tracker,
            &palette_capabilities(),
            &runner,
            &signaller,
        );

        match outcome {
            ApplyOutcome::WriteFailed(failure) => {
                assert!(matches!(
                    failure.cause,
                    WriteFailureCause::GenerateColorsUnrunnable(_)
                ));
                assert_eq!(
                    failure.rolled_back,
                    vec![fs::canonicalize(&monitors).expect("canonicalize")],
                );
            }
            other => panic!("expected WriteFailed, got {other:?}"),
        }
        assert_eq!(
            fs::read(&monitors).expect("read the rolled-back file"),
            b"monitor=eDP-1,preferred,auto,1\n",
        );
    }

    // --- (g) a reload failure: writes stand, error surfaced, others attempted ----

    #[test]
    fn a_reload_failure_is_non_fatal_and_the_write_stands() {
        // Accept criterion (g): a reload that fails does not roll back the write —
        // the write stands and the failure is surfaced (R5.5).
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let monitors = write_file(&dir, "monitors.conf", b"monitor=eDP-1,preferred,auto,1\n");
        let tracker = record_baseline(&[&monitors]);

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: monitors.clone(),
                contents: b"monitor=eDP-1,preferred,auto,1.25\n".to_vec(),
                changed_keys: vec!["monitor:eDP-1 scale".to_string()],
                backing: BackingFile::MonitorsConf,
                validation: WriteValidation::InPlan,
            }],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        // `hyprctl reload` exits non-zero.
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        let signaller = MockProcessSignaller::new();

        let outcome = run(&plan, &tracker, &hyprland_only(), &runner, &signaller);

        match outcome {
            ApplyOutcome::Applied {
                reload_failures,
                written,
                ..
            } => {
                assert_eq!(reload_failures.len(), 1, "the failed reload is surfaced");
                assert!(matches!(
                    reload_failures[0],
                    ReloadError::NonZeroExit { .. }
                ));
                assert_eq!(
                    written,
                    vec![monitors.clone()],
                    "the write is reported as applied even though its reload failed"
                );
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert_eq!(
            fs::read(&monitors).expect("read the file"),
            b"monitor=eDP-1,preferred,auto,1.25\n",
            "the write must stand despite the reload failure (R5.5)"
        );
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").arg("reload")]
        );
    }

    #[test]
    fn other_reloads_are_still_attempted_after_one_fails() {
        // Accept criterion (g): a failing reload does not stop the rest — the whole
        // reload plan is attempted so a healthy component still reloads.
        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: Vec::new(),
            palette: Some(PaletteSwitch {
                scheme: "nord".to_string(),
                generate_colors: PathBuf::from("/fake/generate-colors"),
            }),
            reload_params: ReloadParams::default(),
        };
        let tracker = FreshnessTracker::new();
        // generate-colors OK, then `hyprctl reload` FAILS; eww/swaync default to OK.
        let runner = MockCommandRunner::with_outcomes([
            Ok(CommandOutput::fake(0)),
            Ok(CommandOutput::fake(1)),
        ]);
        let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![7])]);

        let outcome = run(
            &plan,
            &tracker,
            &palette_capabilities(),
            &runner,
            &signaller,
        );

        match outcome {
            ApplyOutcome::Applied {
                reload_failures,
                written,
                ..
            } => {
                assert_eq!(reload_failures.len(), 1, "only the hyprctl reload failed");
                assert!(
                    written.is_empty(),
                    "a palette switch writes no file directly"
                );
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        // Even though `hyprctl reload` failed, eww and swaync were still attempted
        // and kitty was still signalled — the whole plan runs.
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("/fake/generate-colors").arg("nord"),
                Command::new("hyprctl").arg("reload"),
                Command::new("eww").arg("reload"),
                Command::new("swaync-client").arg("-rs"),
            ]
        );
        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: "kitty".to_string(),
                signal: Signal::SIGUSR1,
                pids: vec![7],
            }]
        );
    }

    // --- A combined file write + palette switch (the deduped union) --------------

    #[test]
    fn a_file_write_and_a_palette_switch_run_generate_colors_after_the_write() {
        // A combined apply: rewrite monitors.conf AND switch the palette. The file is
        // written first and generate-colors runs as the last write step (proven
        // strictly by the rollback test above, where a generator failure rolls the
        // file back), and the reload set is the correct deduped union of the file's
        // reload (hyprctl reload) and the palette chain.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let monitors = write_file(&dir, "monitors.conf", b"monitor=eDP-1,preferred,auto,1\n");
        let tracker = record_baseline(&[&monitors]);

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: monitors.clone(),
                contents: b"monitor=eDP-1,preferred,auto,1.25\n".to_vec(),
                changed_keys: vec!["monitor:eDP-1 scale".to_string()],
                backing: BackingFile::MonitorsConf,
                validation: WriteValidation::InPlan,
            }],
            palette: Some(PaletteSwitch {
                scheme: "nord".to_string(),
                generate_colors: PathBuf::from("/fake/repo/scripts/generate-colors"),
            }),
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![9])]);

        let outcome = run(
            &plan,
            &tracker,
            &palette_capabilities(),
            &runner,
            &signaller,
        );

        match outcome {
            ApplyOutcome::Applied {
                reload_failures,
                written,
                ..
            } => {
                assert!(reload_failures.is_empty());
                assert_eq!(written, vec![monitors.clone()]);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        // The file change reached disk.
        assert_eq!(
            fs::read(&monitors).expect("read the file"),
            b"monitor=eDP-1,preferred,auto,1.25\n",
        );
        // generate-colors is recorded first (the last write step, after the file
        // write), then the DEDUPED union of the monitors reload (hyprctl reload) and
        // the palette chain — `hyprctl reload` appears once, not twice.
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("/fake/repo/scripts/generate-colors").arg("nord"),
                Command::new("hyprctl").arg("reload"),
                Command::new("eww").arg("reload"),
                Command::new("swaync-client").arg("-rs"),
            ]
        );
        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: "kitty".to_string(),
                signal: Signal::SIGUSR1,
                pids: vec![9],
            }]
        );
    }

    #[test]
    fn a_palette_switch_plus_a_swaync_config_write_runs_both_swaync_reloads_in_order() {
        // Task 9.9: swaync's config reload (`-R`) and its CSS reload (`-rs`) are two
        // different commands, so an Apply that both switched the palette and edited
        // `swaync/config.json` must execute both — the config reload before the CSS
        // reload — with nothing duplicated. Asserting it here, at the pipeline level,
        // pins the executed command sequence end to end; `core::reload` covers the
        // planning and the individual arg vectors.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let config = write_file(&dir, "config.json", b"{\n  \"positionX\": \"right\"\n}\n");
        let tracker = record_baseline(&[&config]);

        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: config.clone(),
                contents: b"{\n  \"positionX\": \"left\"\n}\n".to_vec(),
                changed_keys: vec!["positionX".to_string()],
                backing: BackingFile::SwayncConfig,
                validation: WriteValidation::InPlan,
            }],
            palette: Some(PaletteSwitch {
                scheme: "nord".to_string(),
                generate_colors: PathBuf::from("/fake/repo/scripts/generate-colors"),
            }),
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::with_running([("kitty".to_string(), vec![11])]);

        let outcome = run(
            &plan,
            &tracker,
            &palette_capabilities(),
            &runner,
            &signaller,
        );

        match outcome {
            ApplyOutcome::Applied {
                reload_failures,
                written,
                ..
            } => {
                assert!(reload_failures.is_empty());
                assert_eq!(written, vec![config.clone()]);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("/fake/repo/scripts/generate-colors").arg("nord"),
                Command::new("hyprctl").arg("reload"),
                Command::new("eww").arg("reload"),
                Command::new("swaync-client").arg("-R"),
                Command::new("swaync-client").arg("-rs"),
            ],
            "both swaync reloads must run, config before CSS, each exactly once"
        );
        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: "kitty".to_string(),
                signal: Signal::SIGUSR1,
                pids: vec![11],
            }]
        );
    }

    // --- The rollback-restore-itself-failing path --------------------------------

    #[cfg(unix)]
    #[test]
    fn roll_back_reports_a_restore_that_itself_fails() {
        // The `Err` arm of `roll_back`: when a snapshot's restore cannot be written
        // (its parent directory is read-only), the file is reported in
        // `rollback_failures` rather than `rolled_back`, so the UI can warn the
        // desktop may be partially changed. This cannot be induced through the full
        // pipeline — a parent directory writable enough for the forward write is
        // writable enough for the restore too — so the private helper is exercised
        // directly with a real snapshot from a real forward write.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let subdir = dir.path().join("conf.d");
        fs::create_dir(&subdir).expect("create the sub-directory");
        let target = subdir.join("a.conf");
        fs::write(&target, b"original\n").expect("write the original file");

        // A real forward write yields a snapshot holding the original bytes.
        let snapshot = write_atomic(&target, b"changed\n").expect("atomic write");
        let resolved = snapshot.resolved_path().to_path_buf();

        // Make the parent directory read-only so the restore's temp-file creation
        // (and hence the atomic rename) fails.
        fs::set_permissions(&subdir, fs::Permissions::from_mode(0o500))
            .expect("make the directory read-only");

        // Guard: only assert when the directory is genuinely non-writable — running
        // as root bypasses the mode and the restore would succeed. Probe by trying
        // to create a file in it, mirroring the guarded permission tests elsewhere.
        let result = if fs::File::create(subdir.join(".probe")).is_ok() {
            None
        } else {
            Some(roll_back(std::slice::from_ref(&snapshot)))
        };

        // Restore write permission so the temp dir can be cleaned up.
        let _ = fs::set_permissions(&subdir, fs::Permissions::from_mode(0o755));

        if let Some((rolled_back, failures)) = result {
            assert!(
                rolled_back.is_empty(),
                "the restore could not complete, so nothing is reported as rolled back"
            );
            assert_eq!(failures.len(), 1, "the failed restore is reported");
            assert_eq!(
                failures[0].0, resolved,
                "the rollback failure names the resolved path"
            );
        }
    }

    // --- Message rendering (the UI shows these verbatim) -------------------------

    #[test]
    fn write_failure_cause_messages_are_human_readable() {
        // Exercise every WriteFailureCause arm's Display so the UI never shows an
        // empty message, and so every variant is constructed.
        let cases = vec![
            WriteFailureCause::File {
                path: PathBuf::from("/x.conf"),
                error: WriteError::Resolve {
                    target: PathBuf::from("/x.conf"),
                    source: io::Error::from(io::ErrorKind::NotFound),
                },
            },
            WriteFailureCause::GenerateColorsExit {
                code: Some(1),
                stderr_excerpt: None,
            },
            WriteFailureCause::GenerateColorsExit {
                code: None,
                stderr_excerpt: Some("killed while writing colors.conf".to_string()),
            },
            WriteFailureCause::GenerateColorsUnrunnable(CommandError::Timeout {
                limit: std::time::Duration::from_secs(5),
            }),
        ];
        for cause in cases {
            assert!(
                !cause.to_string().is_empty(),
                "every WriteFailureCause must render a message, got empty for {cause:?}"
            );
        }
    }

    // --- The caller-drift guard on writes without validations (task 9.10) --------

    /// A plan with one write per `(backing, validation)` pair and no `validations` at
    /// all — the shape the drift guard reacts to.
    fn plan_with_writes(writes: &[(BackingFile, WriteValidation)]) -> ApplyPlan {
        ApplyPlan {
            validations: Vec::new(),
            writes: writes
                .iter()
                .map(|(backing, validation)| FileWrite {
                    path: PathBuf::from("/fake/target.conf"),
                    contents: b"contents\n".to_vec(),
                    changed_keys: vec![format!("{backing:?} key")],
                    backing: *backing,
                    validation: *validation,
                })
                .collect(),
            palette: None,
            reload_params: ReloadParams::default(),
        }
    }

    /// Captures the `tracing` events `measured` emits, discarding those from `warm_up`.
    ///
    /// `warm_up` must reach the same callsites as `measured`: `tracing` caches each
    /// callsite's interest process-globally and cross-thread, so a parallel test that
    /// reached the guard's `warn!` under the no-op default subscriber could have cached
    /// it as "never" and suppressed the event here. Touching the callsites once,
    /// rebuilding the interest cache, then clearing before measuring is the same guard
    /// `core::detect`'s and `parsers::hyprlang`'s log tests use. The subscriber is
    /// installed for this thread only, so no global logging state is touched.
    fn captured_events(
        warm_up: impl FnOnce(),
        measured: impl FnOnce(),
    ) -> Vec<(tracing::Level, String)> {
        use std::sync::{Arc, Mutex};
        use tracing::subscriber::with_default;
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;

        /// Collects each event's level and its rendered fields.
        #[derive(Clone, Default)]
        struct LogCapture {
            events: Arc<Mutex<Vec<(tracing::Level, String)>>>,
        }

        /// Renders every field of an event into one string — the message *and* the
        /// structured fields, since the changed-key labels are recorded as a field.
        struct FieldVisitor<'a>(&'a mut String);
        impl tracing::field::Visit for FieldVisitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                use std::fmt::Write as _;
                let _ = write!(self.0, " {}={value:?}", field.name());
            }
        }

        impl<S: tracing::Subscriber> Layer<S> for LogCapture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut rendered = String::new();
                event.record(&mut FieldVisitor(&mut rendered));
                self.events
                    .lock()
                    .expect("log capture mutex should not be poisoned")
                    .push((*event.metadata().level(), rendered));
            }
        }

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        with_default(subscriber, || {
            warm_up();
            tracing::callsite::rebuild_interest_cache();
            capture
                .events
                .lock()
                .expect("log capture mutex should not be poisoned")
                .clear();
            measured();
        });
        capture
            .events
            .lock()
            .expect("log capture mutex should not be poisoned")
            .clone()
    }

    /// The `WARN`-level events among `events`, which is all the drift guard emits.
    fn warnings(events: &[(tracing::Level, String)]) -> Vec<&str> {
        events
            .iter()
            .filter(|(level, _)| *level == tracing::Level::WARN)
            .map(|(_, rendered)| rendered.as_str())
            .collect()
    }

    #[test]
    fn a_plan_with_writes_and_no_validations_is_flagged_as_drifted() {
        // The drift the guard exists for: a caller rendered a write whose values it
        // claims the plan validates, but dropped them from `validations` — so step 1's
        // R8.3 gate would pass vacuously.
        let plan = plan_with_writes(&[(BackingFile::MonitorsConf, WriteValidation::InPlan)]);
        let flagged = writes_without_validations(&plan);
        assert_eq!(flagged.len(), 1, "the unvalidated write must be flagged");
        assert_eq!(flagged[0].backing, BackingFile::MonitorsConf);

        // Supplying the validation clears it — a consistent plan is never flagged.
        let mut validated =
            plan_with_writes(&[(BackingFile::MonitorsConf, WriteValidation::InPlan)]);
        validated
            .validations
            .push((SettingId::MonitorScale, Value::Float(1.25)));
        assert!(writes_without_validations(&validated).is_empty());
    }

    #[test]
    fn a_by_design_validation_free_plan_is_not_flagged_but_a_mixed_one_is() {
        // The declaration that keeps the guard honest. Two real applies reach the
        // pipeline with writes and no validations by design: a theme-only change (the
        // GTK/icon/cursor model carries no validations at all — task 9.27) and a
        // wallpaper *fit-only* change, which must not drag the unchanged wallpaper path
        // into validation. Both must stay quiet, or the warning fires on ordinary applies
        // and becomes worth ignoring.
        let by_design = plan_with_writes(&[
            (BackingFile::GtkSettings, WriteValidation::NotNeeded),
            (BackingFile::UwsmEnv, WriteValidation::NotNeeded),
            (BackingFile::HyprlandConf, WriteValidation::NotNeeded),
            (BackingFile::HyprpaperConf, WriteValidation::NotNeeded),
        ]);
        assert!(
            writes_without_validations(&by_design).is_empty(),
            "writes that declare themselves validation-free must not be flagged"
        );

        // A write that does claim plan validation, alongside by-design ones, is still
        // flagged — and only it: the declaration must not launder a genuine drift.
        let mixed = plan_with_writes(&[
            (BackingFile::GtkSettings, WriteValidation::NotNeeded),
            (BackingFile::HypridleConf, WriteValidation::InPlan),
        ]);
        let flagged = writes_without_validations(&mixed);
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].backing, BackingFile::HypridleConf);

        // A palette-only apply has no writes at all, so there is nothing to flag.
        let palette_only = ApplyPlan {
            validations: Vec::new(),
            writes: Vec::new(),
            palette: Some(PaletteSwitch {
                scheme: "nord".to_string(),
                generate_colors: PathBuf::from("/fake/generate-colors"),
            }),
            reload_params: ReloadParams::default(),
        };
        assert!(writes_without_validations(&palette_only).is_empty());
    }

    #[test]
    fn a_drifted_plan_logs_a_warning_naming_the_unvalidated_keys() {
        // The guard's whole effect is the log line (it deliberately does not abort the
        // apply — see `warn_on_missing_validations` on warn vs debug_assert), so pin that
        // the line is emitted at `warn` and names the write's changed keys, which is what
        // makes it actionable in the journal (R7.3).
        let mut plan = plan_with_writes(&[(BackingFile::HypridleConf, WriteValidation::InPlan)]);
        plan.writes[0].changed_keys = vec!["listener:timeout".to_string()];
        let by_design = plan_with_writes(&[(BackingFile::GtkSettings, WriteValidation::NotNeeded)]);

        let events = captured_events(
            || warn_on_missing_validations(&plan),
            || {
                warn_on_missing_validations(&plan);
                // The by-design plan must add nothing at all to the journal.
                warn_on_missing_validations(&by_design);
            },
        );

        let warnings = warnings(&events);
        assert_eq!(
            warnings.len(),
            1,
            "exactly the drifted plan warns; the by-design one is silent: {events:?}"
        );
        assert!(
            warnings[0].contains("listener:timeout"),
            "the warning names the unvalidated write's changed keys: {}",
            warnings[0]
        );
    }

    #[test]
    fn run_warns_about_a_drifted_plan_through_the_pipeline_entry_point() {
        // The guard is only worth anything if it is actually wired into `run`: the test
        // above exercises the helper, this one goes through the real entry point the UI
        // calls, so deleting the call from `run` fails the suite rather than merely
        // tripping the dead-code lint. It also pins that the guard *only* warns — the
        // apply proceeds and the write stands, because a suspicion must never abort an
        // otherwise valid apply (R8.3).
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let hypridle = write_file(&dir, "hypridle.conf", b"general {\n    lock_cmd = old\n}\n");
        let new_bytes = b"general {\n    lock_cmd = new\n}\n".to_vec();

        // A drifted plan: a write claiming plan-validated values, with none supplied.
        let plan = ApplyPlan {
            validations: Vec::new(),
            writes: vec![FileWrite {
                path: hypridle.clone(),
                contents: new_bytes.clone(),
                changed_keys: vec!["general:lock_cmd".to_string()],
                backing: BackingFile::HypridleConf,
                validation: WriteValidation::InPlan,
            }],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        // hypridle absent, so the reload plan is empty: the only command-related logging
        // is gone and any WARN captured below must come from the guard itself.
        let caps = Capabilities::for_tests(&[], &[], false);
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        let mut outcome = None;
        let events = captured_events(
            || {
                let tracker = record_baseline(&[&hypridle]);
                let _ = run(&plan, &tracker, &caps, &runner, &signaller);
            },
            || {
                // Re-baseline: the warm-up run above already rewrote the file, so a
                // tracker recorded before it would now report a (self-inflicted) conflict.
                let tracker = record_baseline(&[&hypridle]);
                outcome = Some(run(&plan, &tracker, &caps, &runner, &signaller));
            },
        );

        let warnings = warnings(&events);
        assert_eq!(
            warnings.len(),
            1,
            "`run` must emit the drift warning exactly once: {events:?}"
        );
        assert!(
            warnings[0].contains("general:lock_cmd"),
            "the warning from `run` names the unvalidated write: {}",
            warnings[0]
        );
        assert!(
            matches!(outcome, Some(ApplyOutcome::Applied { .. })),
            "the guard must warn without aborting the apply, got {outcome:?}"
        );
        assert_eq!(
            fs::read(&hypridle).expect("read the written file"),
            new_bytes,
            "the write still stands: the guard is a diagnostic, not a gate"
        );
        assert!(
            runner.recorded().is_empty(),
            "no reload is planned for an absent hypridle, so no command noise"
        );
    }

    // --- The store's freshness tracker wired into apply (task 5.4) ---------------

    #[test]
    fn a_second_apply_after_commit_is_not_a_self_conflict() {
        // Task 5.4 wires the store's real freshness tracker (populated by the startup
        // load) into `run` via `SettingsStore::freshness`, replacing the empty tracker
        // the interim 5.3 wiring used. This exercises the loop the task-4.5 commit
        // contract closes: after a successful apply the caller commits — promoting
        // staged→original and re-baselining the written file from the exact bytes —
        // so the app's own write is NOT mistaken for an external edit on the next
        // apply's step-2 conflict check.
        use crate::core::store::{FileReader, FileValues, SettingsStore};

        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        let path = write_file(&dir, "input.conf", b"input {\n  sensitivity = 0.0\n}\n");

        // Load the file into a real store, establishing the freshness baseline from
        // the exact bytes read (as the startup loader does). The reader is trivial
        // here: these assertions never trigger a conflict reload.
        let reader: FileReader = Box::new(|p: &Path| {
            let bytes = fs::read(p)?;
            Ok(FileValues {
                bytes,
                values: Vec::new(),
            })
        });
        let mut store = SettingsStore::new();
        let bytes = fs::read(&path).expect("read the fixture");
        store.load_file(
            &path,
            FileValues {
                bytes,
                values: vec![(SettingId::MouseSensitivity, Value::Float(0.0))],
            },
            reader,
        );
        store
            .stage(SettingId::MouseSensitivity, Value::Float(0.5))
            .expect("a valid edit stages");

        let new_bytes = b"input {\n  sensitivity = 0.5\n}\n".to_vec();
        let plan = ApplyPlan {
            validations: vec![(SettingId::MouseSensitivity, Value::Float(0.5))],
            writes: vec![FileWrite {
                path: path.clone(),
                contents: new_bytes.clone(),
                changed_keys: vec!["input:sensitivity".to_string()],
                backing: BackingFile::InputConf,
                validation: WriteValidation::InPlan,
            }],
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        // No hyprctl/live IPC, so the InputConf reload is gated out — this test is
        // about the conflict check, not the reload set.
        let caps = Capabilities::for_tests(&[], &[], false);

        // First apply: the baseline still matches disk, so no conflict; the file is
        // written and reported for the caller to commit.
        let written = match run(&plan, store.freshness(), &caps, &runner, &signaller) {
            ApplyOutcome::Applied { written, .. } => written,
            other => panic!("expected Applied on the first apply, got {other:?}"),
        };
        assert_eq!(written, vec![path.clone()]);

        // Commit reconciles the store: promote staged→original and re-baseline the
        // written file from the exact bytes written.
        let committed: Vec<(PathBuf, Vec<u8>)> = written
            .into_iter()
            .map(|p| (p, new_bytes.clone()))
            .collect();
        store.commit_apply(&committed);

        // Second apply over the store's now-re-baselined tracker: the on-disk bytes
        // are the app's own write, which must NOT read as an external conflict.
        let empty_plan = ApplyPlan {
            validations: Vec::new(),
            writes: Vec::new(),
            palette: None,
            reload_params: ReloadParams::default(),
        };
        let outcome = run(&empty_plan, store.freshness(), &caps, &runner, &signaller);
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "a second apply after commit must not self-conflict, got {outcome:?}"
        );
    }
}
