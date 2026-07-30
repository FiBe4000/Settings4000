//! Assembling one [`ApplyPlan`] out of every staging source (task 9.16; architecture
//! §6; R5.3–R5.6, R6.2, R8.3).
//!
//! # Why this is not in the window
//!
//! An Apply has two halves. The *back* half — validate everything, conflict-check,
//! write atomically with rollback, reload what changed — is
//! [`apply::run`](crate::core::apply::run). The *front* half is the assembly this module
//! performs: ask every staging source (the shared
//! [`SettingsStore`] plus the bespoke page models) what it wants written, fold the
//! answers into one plan in a fixed order, and refuse to plan anything at all when a
//! source cannot be prepared or has been overtaken by an external edit.
//!
//! That front half used to sit inside the window's Apply-button closure, where the only
//! way to exercise it was to click the button — against R6.2's intent, and a poor fit
//! for decisions whose failure paths are near-unreachable in normal use (an unreadable
//! backing file, a record the writer cannot edit). Here it is GTK-free and covered
//! headlessly by `tests/apply_assembly.rs`.
//!
//! # The decisions this module owns
//!
//! 1. **The model-owned conflict guards run first, before any plan is built** (R5.6).
//!    The Display and the two Theme models read backing files the store does not track,
//!    so the pipeline's own step-2 conflict check cannot cover them; each is asked
//!    whether its files changed on disk since it read them, and the first one that says
//!    yes aborts the Apply with [`PrepareFailure::Conflict`]. Nothing is written and
//!    nothing is committed, so the window can reload that model and let the user
//!    re-apply.
//! 2. **The store-backed writes are captured before the model contributions are folded
//!    in.** [`AssembledPlan::store_writes`] is what the window hands
//!    [`SettingsStore::commit_apply`] afterwards, and it must list *only* the files
//!    whose freshness the store owns. The bespoke models own (and re-baseline) their own
//!    files, so their writes must not appear in that list — see the field docs.
//! 3. **A write that cannot be prepared aborts the whole Apply** with
//!    [`PrepareFailure::Write`], rather than being skipped. Skipping would let the Apply
//!    succeed for everything else and then commit the staged values of the skipped page
//!    against a file that was never written, silently desyncing the app from disk. The
//!    staged edits survive every abort path: nothing here mutates a source.
//!
//! # What stays in the window
//!
//! Everything with a side effect or a widget: running the pipeline, reloading a
//! conflicted model (the reload is per-model — the Display model needs a
//! [`CommandRunner`](crate::system::command::CommandRunner) and can come back empty,
//! the Theme models cannot), re-rendering pages, showing the dialog or toast, and
//! committing the store and the models after an
//! [`Applied`](crate::core::apply::ApplyOutcome::Applied) outcome. The assembler only
//! reports *which* source failed and *which* models contributed
//! ([`AssembledPlan::commits`]); it never phrases user-facing text (that is
//! `ui::chrome`'s job) and never touches a source mutably.

use std::fmt;
use std::path::PathBuf;

use crate::core::apply::{ApplyPlan, FileWrite};
use crate::core::display::DisplayModel;
use crate::core::input::InputModel;
use crate::core::model::{Category, SettingId, Value};
use crate::core::notifications::NotificationsModel;
use crate::core::power::PowerModel;
use crate::core::reload::ReloadParams;
use crate::core::store::SettingsStore;
use crate::core::theme::{PaletteModel, ThemesModel, WallpaperModel};

/// The staging sources an Apply is assembled from — the shared store plus every bespoke
/// page model the startup load managed to build.
///
/// Each model is `Option` because its page degrades to absent when the app or config it
/// needs is missing (R4.2/R4.4): no live compositor means no [`DisplayModel`], no
/// `gsettings` means no [`ThemesModel`], and so on. An absent source simply contributes
/// nothing.
///
/// The models are taken by shared reference: assembly is read-only, which is what lets
/// the window hold all of them borrowed at once and what makes "the staged edits survive
/// an abort" true by construction rather than by care.
pub struct ApplySources<'a> {
    /// The single staging store every framework row edits (R5.1). Supplies the plan's
    /// R8.3 validations and the dirty values the store-backed writes render from.
    pub store: &'a SettingsStore,
    /// The Display page's model (task 6.1): stages monitor edits and renders the
    /// `monitors.conf` write, and owns that file's freshness itself.
    pub display: Option<&'a DisplayModel>,
    /// The Input page's helper (task 6.6): renders the store's dirty Input settings into
    /// the `input.conf` write. Not a staging source of its own.
    pub input: Option<&'a InputModel>,
    /// The Notifications page's helper (task 6.7): renders the store's dirty
    /// position/timeout settings into the swaync `config.json` write. Not a staging
    /// source of its own (the Do-Not-Disturb switch is runtime-only, R5.2).
    pub notifications: Option<&'a NotificationsModel>,
    /// The Power & Idle page's helper (task 6.8): renders the store's dirty timeouts and
    /// lock command into the `hypridle.conf` write. Not a staging source of its own.
    pub power: Option<&'a PowerModel>,
    /// The Theme page's palette model (task 6.3): stages a color-scheme switch, which
    /// runs `generate-colors` as the pipeline's last write step.
    pub palette: Option<&'a PaletteModel>,
    /// The Theme page's GTK/icon/cursor model (task 6.4): stages theme names and the
    /// cursor theme+size, writes every duplicated copy identically (R3.4), and owns its
    /// backing files' freshness itself.
    pub themes: Option<&'a ThemesModel>,
    /// The Theme page's wallpaper / lock-background model (task 6.5): stages image paths
    /// and the fit mode, and owns `hyprpaper.conf`/`hyprlock.conf` freshness itself.
    pub wallpaper: Option<&'a WallpaperModel>,
}

impl<'a> ApplySources<'a> {
    /// The sources of an app in which no bespoke page model exists — only the shared
    /// store.
    ///
    /// This is the genuine startup state (every model is `None` until the worker builds
    /// it, and stays `None` for a page whose app is absent), so it doubles as the base a
    /// caller fills in with struct-update syntax:
    ///
    /// ```text
    /// ApplySources { input: Some(&input), ..ApplySources::for_store(&store) }
    /// ```
    ///
    /// (Shown as text rather than a doctest: the snippet needs a loaded store and a built
    /// model to compile, which is what the assembly suite already sets up properly.)
    pub fn for_store(store: &'a SettingsStore) -> Self {
        ApplySources {
            store,
            display: None,
            input: None,
            notifications: None,
            power: None,
            palette: None,
            themes: None,
            wallpaper: None,
        }
    }
}

/// A staging source that owns its backing files' freshness and found them changed on
/// disk (R5.6).
///
/// Only these three exist: the store's own files are conflict-checked by the pipeline
/// itself (step 2), so a source appears here exactly when *it*, not the store, tracks
/// the file it would write. The window uses the variant to pick which model to reload
/// from disk and which page to re-render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictedSource {
    /// The Display model's `monitors.conf` (task 6.1).
    Display,
    /// The GTK/icon/cursor model's backing files — both `settings.ini` files,
    /// `hyprland.conf`, and `uwsm/env` (task 6.4).
    Themes,
    /// The wallpaper model's `hyprpaper.conf`/`hyprlock.conf` (task 6.5).
    Wallpaper,
}

/// The write whose preparation failed, naming the page whose edits were kept.
///
/// One variant per source that can fail while *rendering* its write: the three
/// store-backed pages (the file is unreadable, or the writer rejects an edit) and the
/// Display model (a record it cannot edit in place). The remaining contributions cannot
/// fail — a palette switch runs a script and the two Theme models skip a value they
/// cannot write (task 9.6), which task 9.28 will surface separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailedWrite {
    /// The Display model's `monitors.conf` write (task 6.1).
    Display,
    /// The Input page's `input.conf` write (task 6.6).
    Input,
    /// The Notifications page's swaync `config.json` write (task 6.7).
    Notifications,
    /// The Power & Idle page's `hypridle.conf` write (task 6.8).
    Power,
}

/// Why no plan could be assembled — in both cases nothing was written, nothing was
/// committed, and every staged edit is still there for a retry.
///
/// `ui::chrome` turns this into the dialog text and the window into the recovery action
/// (a conflict additionally reloads the affected model).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrepareFailure {
    /// A model-owned backing file changed on disk since the model read it (R5.6). The
    /// window reloads that model rather than writing over the external edit.
    Conflict(ConflictedSource),
    /// A pending edit's write could not be prepared at all.
    Write {
        /// Which write failed, i.e. which page's edits were kept.
        source: FailedWrite,
        /// The failure, already rendered through the underlying error's
        /// [`Display`](fmt::Display) — the assembler logs it and the UI quotes it.
        ///
        /// The message is carried as text rather than as the typed error because the four
        /// sources have four unrelated error types and nothing downstream distinguishes
        /// them: the log line and the dialog both only quote the reason.
        message: String,
    },
}

/// Which bespoke models contributed to the plan and therefore must be committed after an
/// [`Applied`](crate::core::apply::ApplyOutcome::Applied) outcome.
///
/// The store is committed unconditionally from [`AssembledPlan::store_writes`], but each
/// bespoke model must be committed **only** when it actually contributed: committing a
/// model that wrote nothing would promote its staged edits (or re-baseline its files)
/// against contents it never produced. The flags are computed while folding, so they
/// cannot drift from what was folded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelCommits {
    /// The Display model contributed its `monitors.conf` write (task 6.1).
    pub display: bool,
    /// The palette model contributed a scheme switch (task 6.3).
    pub palette: bool,
    /// The GTK/icon/cursor model contributed at least one write (task 6.4).
    pub themes: bool,
    /// The wallpaper model contributed at least one write (task 6.5).
    pub wallpaper: bool,
}

/// A plan ready to run, plus what the caller needs to reconcile the staging sources
/// afterwards.
#[derive(Clone, Debug)]
pub struct AssembledPlan {
    /// The plan to hand [`apply::run`](crate::core::apply::run).
    pub plan: ApplyPlan,
    /// The `(path, contents)` pairs of the writes whose freshness the **store** owns —
    /// the `input.conf`, swaync `config.json`, and `hypridle.conf` writes — for
    /// [`SettingsStore::commit_apply`].
    ///
    /// Deliberately not every write in the plan. `commit_apply` re-baselines each listed
    /// file's freshness from the bytes just written, so that the app's own write is not
    /// seen as an external conflict next time; the bespoke models do that for their own
    /// files in their own `commit()`. Listing a model's file here as well would have the
    /// store re-baseline a file it never loaded, and listing none of the store's files
    /// would leave the next Apply spuriously conflicted (task 4.5).
    pub store_writes: Vec<(PathBuf, Vec<u8>)>,
    /// Which bespoke models must be committed on success — see [`ModelCommits`].
    pub commits: ModelCommits,
}

/// Assembles the Apply plan from every staging source, or reports why no Apply may run
/// (task 9.16).
///
/// The order is fixed and load-bearing; see the module docs for why each step is where
/// it is:
///
/// 1. the model-owned conflict guards (R5.6) — the first stale source returns
///    [`PrepareFailure::Conflict`];
/// 2. the store's dirty values as the plan's R8.3 validations
///    ([`base_apply_plan`]);
/// 3. the three store-backed file writes, in category order;
/// 4. the store-write snapshot for the later commit;
/// 5. the four bespoke model contributions (Display, palette, GTK/icon/cursor themes,
///    wallpaper).
///
/// Any failure in steps 3 or 5 aborts with [`PrepareFailure::Write`] before the caller
/// can run the pipeline, so nothing is written and nothing is committed.
pub fn assemble_apply_plan(sources: ApplySources<'_>) -> Result<AssembledPlan, PrepareFailure> {
    // Step 1: the models that own their backing files' freshness check it themselves,
    // before anything is built — a conflict here means this Apply must not write at all.
    if let Some(source) = first_conflicted_source(&sources) {
        return Err(PrepareFailure::Conflict(source));
    }

    // Step 2: the store's dirty settings, as the values the pipeline re-validates (R8.3).
    let mut plan = base_apply_plan(sources.store);

    // Step 3: one surgical write per store-backed page, rendered from that page's dirty
    // store settings. Folded before the snapshot below so `commit_apply` re-baselines
    // them (their freshness lives in the store's tracker, loaded at startup).
    fold_store_write(
        &mut plan,
        sources.store,
        Category::Input,
        sources.input,
        InputModel::input_conf_write,
        FailedWrite::Input,
    )?;
    fold_store_write(
        &mut plan,
        sources.store,
        Category::Notifications,
        sources.notifications,
        NotificationsModel::swaync_config_write,
        FailedWrite::Notifications,
    )?;
    fold_store_write(
        &mut plan,
        sources.store,
        Category::PowerAndIdle,
        sources.power,
        PowerModel::hypridle_conf_write,
        FailedWrite::Power,
    )?;

    // Step 4: snapshot the store-owned writes before the bespoke models add theirs (see
    // `AssembledPlan::store_writes`).
    let store_writes: Vec<(PathBuf, Vec<u8>)> = plan
        .writes
        .iter()
        .map(|write| (write.path.clone(), write.contents.clone()))
        .collect();

    // Step 5: the bespoke models. Their shapes genuinely differ — only the Display
    // contribution can fail, only the palette one is not a file write, and each carries a
    // different mix of validations and reload parameters — so they are folded one by one
    // rather than through a shared helper that would have to accept all of it as optional.
    let mut commits = ModelCommits::default();

    // The Display page's `monitors.conf` write plus the staged monitor values to
    // re-validate (task 6.1; R8.3).
    let display_contribution = match sources.display {
        Some(model) => model
            .apply_contribution()
            .map_err(|error| write_failure(FailedWrite::Display, error))?,
        None => None,
    };
    if let Some(contribution) = display_contribution {
        plan.writes.push(contribution.write);
        plan.validations.extend(contribution.validations);
        commits.display = true;
    }

    // The palette switch (task 6.3): it writes no file directly — the pipeline runs the
    // discovered `generate-colors <scheme>` as its last write step, then the palette
    // reload chain — because v1 never edits `colors/<scheme>`.
    if let Some(switch) = sources.palette.and_then(PaletteModel::apply_contribution) {
        plan.palette = Some(switch);
        commits.palette = true;
    }

    // The GTK/icon/cursor theme writes (task 6.4): the value goes identically to every
    // copy the app owns (both `settings.ini` files and, for the cursor, `hyprland.conf`'s
    // env lines and `uwsm/env` — R3.4), and the reload parameters drive `gsettings set`
    // plus `hyprctl setcursor`.
    if let Some(contribution) = sources.themes.and_then(ThemesModel::apply_contribution) {
        plan.writes.extend(contribution.writes);
        merge_reload_params(&mut plan.reload_params, contribution.reload_params);
        commits.themes = true;
    }

    // The wallpaper / lock-background writes (task 6.5): `hyprpaper.conf` (path + fit)
    // and/or `hyprlock.conf` (the lock background), the chosen paths re-validated by the
    // pipeline (R8.3), and the wallpaper reload parameter — which is present only when
    // `hyprpaper.conf` changed, since a hyprlock-only change reloads nothing.
    if let Some(contribution) = sources
        .wallpaper
        .and_then(WallpaperModel::apply_contribution)
    {
        plan.writes.extend(contribution.writes);
        plan.validations.extend(contribution.validations);
        merge_reload_params(&mut plan.reload_params, contribution.reload_params);
        commits.wallpaper = true;
    }

    Ok(AssembledPlan {
        plan,
        store_writes,
        commits,
    })
}

/// Builds the base [`ApplyPlan`] from the store's dirty edits (task 5.3).
///
/// It carries the store's dirty settings as `validations`, so the first gate of
/// [`apply::run`](crate::core::apply::run) re-checks them (R8.3). It produces **no**
/// `writes` itself: turning a staged [`Value`] into concrete file bytes goes through the
/// format parsers and is per-page glue, folded in by [`assemble_apply_plan`] — so this
/// stays a thin shared starting point.
///
/// Public (rather than an implementation detail of the assembler) so the integration
/// suites can start their plans from the app's real builder: they fold in a single page's
/// write by hand to keep each suite readable, and re-exposing this is what stops them
/// re-implementing the validations half.
pub fn base_apply_plan(store: &SettingsStore) -> ApplyPlan {
    let validations = store
        .dirty_ids()
        .into_iter()
        .filter_map(|id| store.value(id).cloned().map(|value| (id, value)))
        .collect();
    ApplyPlan {
        validations,
        writes: Vec::new(),
        palette: None,
        reload_params: ReloadParams::default(),
    }
}

/// The first staging source with a pending edit whose backing files changed on disk since
/// it read them, or `None` when every one of them is up to date (R5.6).
///
/// Two properties are deliberate. The checks are **ordered and short-circuiting**: each
/// one re-reads files from disk, so a source after the first conflict is never touched —
/// only one conflict can be reported and recovered from at a time anyway. And each is
/// gated on the model being **dirty**: a file changed behind a clean model would write
/// nothing, so blocking an unrelated page's Apply over it would be an Apply the user
/// cannot ever complete. (The pipeline reports such untouched-but-changed *store* files
/// separately and non-blockingly — task 9.11.)
fn first_conflicted_source(sources: &ApplySources<'_>) -> Option<ConflictedSource> {
    if sources
        .display
        .is_some_and(|model| model.is_dirty() && model.check_conflict())
    {
        return Some(ConflictedSource::Display);
    }
    if sources
        .themes
        .is_some_and(|model| model.is_dirty() && model.check_conflict())
    {
        return Some(ConflictedSource::Themes);
    }
    if sources
        .wallpaper
        .is_some_and(|model| model.is_dirty() && model.check_conflict())
    {
        return Some(ConflictedSource::Wallpaper);
    }
    None
}

/// Renders one store-backed page's dirty settings into a [`FileWrite`] and pushes it onto
/// the plan, or aborts the Apply if the write cannot be prepared.
///
/// The three store-backed pages (Input, Notifications, Power & Idle) share this exact
/// shape: take that category's dirty settings from the store, hand them to the page's
/// renderer, and treat the three possible answers as "nothing to write" (the common clean
/// case), "one write" or "abort" — which is why they are one helper rather than three
/// blocks. `render` is the page's renderer (e.g.
/// [`InputModel::input_conf_write`]); `source` names the page in the resulting failure.
///
/// An absent `model` returns without consulting the store: a page whose app is not
/// installed has no settings loaded, so there is nothing dirty to render either way.
fn fold_store_write<M, E: fmt::Display>(
    plan: &mut ApplyPlan,
    store: &SettingsStore,
    category: Category,
    model: Option<&M>,
    render: impl FnOnce(&M, &[(SettingId, Value)]) -> Result<Option<FileWrite>, E>,
    source: FailedWrite,
) -> Result<(), PrepareFailure> {
    let Some(model) = model else {
        return Ok(());
    };
    match render(model, &store.dirty_in_category(category)) {
        Ok(Some(write)) => {
            plan.writes.push(write);
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(error) => Err(write_failure(source, error)),
    }
}

/// Logs an aborted Apply and wraps the underlying error into a [`PrepareFailure::Write`].
///
/// The four log messages are spelled out per source rather than composed from one format
/// string, so each stays a literal that can be grepped from a journal entry straight back
/// to the write it describes (R7.3). The page model has already logged the underlying
/// cause with its path; this line records the *consequence*, which is what the reader of
/// the journal needs: the whole Apply stopped and nothing was written.
fn write_failure(source: FailedWrite, error: impl fmt::Display) -> PrepareFailure {
    match source {
        FailedWrite::Display => {
            tracing::error!(%error, "aborting apply: could not prepare the monitors.conf write")
        }
        FailedWrite::Input => {
            tracing::error!(%error, "aborting apply: could not prepare the input.conf write")
        }
        FailedWrite::Notifications => {
            tracing::error!(%error, "aborting apply: could not prepare the swaync config.json write")
        }
        FailedWrite::Power => {
            tracing::error!(%error, "aborting apply: could not prepare the hypridle.conf write")
        }
    }
    PrepareFailure::Write {
        source,
        message: error.to_string(),
    }
}

/// Merges a page's reload parameters into the plan's, setting each field only when the
/// contribution provides it (task 6.4).
///
/// Both Theme sub-features fill [`ReloadParams`]: the GTK/icon/cursor model (task 6.4)
/// the theme names and cursor theme+size, and the wallpaper model (task 6.5) the
/// wallpaper path and fit mode. A field is overwritten only when `Some`, so a value one
/// contribution sets is never clobbered by another that leaves it `None`. The plan
/// starts from [`ReloadParams::default`] (all `None`), so this composes both cleanly.
fn merge_reload_params(target: &mut ReloadParams, from: ReloadParams) {
    if from.gtk_theme.is_some() {
        target.gtk_theme = from.gtk_theme;
    }
    if from.icon_theme.is_some() {
        target.icon_theme = from.icon_theme;
    }
    if from.cursor.is_some() {
        target.cursor = from.cursor;
    }
    if from.wallpaper.is_some() {
        target.wallpaper = from.wallpaper;
    }
    if from.fit.is_some() {
        target.fit = from.fit;
    }
}
