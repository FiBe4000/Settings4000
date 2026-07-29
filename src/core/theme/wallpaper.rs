//! The wallpaper / lock-screen background staging model for the Theme page (task 6.5;
//! R4.2, R4.4, R5.6, R8.3) — see [`super`] for how the page's three models fit together.
//!
//! The desktop wallpaper (`hyprpaper.conf`) and the lock-screen background
//! (`hyprlock.conf`) both default to the *same* image in the dotfiles (analysis §6.2), so
//! the page presents one wallpaper path plus a fit-mode drop-down and an optional "use a
//! different lock-screen image" override: with no override the same path is written to
//! both files; with one, the wallpaper goes to `hyprpaper.conf` and the override to
//! `hyprlock.conf`. A wallpaper change reloads live via `hyprctl hyprpaper
//! preload`/`wallpaper`, while a hyprlock-only change issues **no** reload (hyprlock
//! reads its config only at the next lock — intentional, architecture §6). Chosen image
//! paths are validated (exists + readable + image extension, R8.3) before staging.
//! [`WallpaperModel`]'s own docs carry the rest.

use std::path::{Path, PathBuf};

use crate::core::apply::{FileWrite, WriteValidation};
use crate::core::freshness::FreshnessTracker;
use crate::core::model::{SettingId, ValidationError, Value, validate_image_path};
use crate::core::reload::{BackingFile, ReloadParams};
use crate::parsers::hyprlang::{HyprlangFile, KeyPath};

use super::{BackingText, PathField, Selection, read_backing};

/// The `hyprpaper.conf` section holding the wallpaper (`wallpaper { path = … }`).
const WALLPAPER_SECTION: &str = "wallpaper";
/// The key naming the wallpaper image path inside the `wallpaper` section.
const WALLPAPER_PATH_KEY: &str = "path";
/// The key naming the wallpaper fit mode inside the `wallpaper` section (analysis §4:
/// `fit_mode = cover`).
const WALLPAPER_FIT_KEY: &str = "fit_mode";
/// The `hyprlock.conf` section holding the lock-screen background
/// (`background { path = … }`).
const LOCK_SECTION: &str = "background";
/// The key naming the lock-screen background image path inside the `background` section.
const LOCK_PATH_KEY: &str = "path";

/// Fit modes offered in the fit-mode drop-down, in a sensible order.
///
/// These are hyprpaper's genuine wallpaper rendering modes: `cover` (fill the output,
/// preserving aspect ratio and cropping — the dotfiles' documented value, analysis §4),
/// `contain` (fit the whole image, letterboxing), `fill` (stretch to fill, ignoring
/// aspect ratio), and `tile` (repeat). The currently-configured value is always kept
/// selectable (see [`WallpaperModel::load`]), mirroring the cursor-size and
/// monitor-scale drop-downs, so an unusual on-disk value is never silently dropped.
const CURATED_FIT_MODES: &[&str] = &["cover", "contain", "fill", "tile"];

/// The live XDG paths of the two config files a wallpaper / lock-background change
/// writes (R8.5).
///
/// Injected (like [`ThemesPaths`](super::ThemesPaths)) so the model is exercised against
/// a fixture tree in tests; the writer follows symlinks, so a dotfiles-deployed file is
/// handled identically to a plain one.
#[derive(Clone, Debug)]
pub struct WallpaperPaths {
    /// `~/.config/hypr/hyprpaper.conf` (the wallpaper `path` and `fit_mode`).
    pub hyprpaper_conf: PathBuf,
    /// `~/.config/hypr/hyprlock.conf` (the lock-screen background `path`).
    pub hyprlock_conf: PathBuf,
}

/// Which of the wallpaper page's staged values a render actually managed to write into a
/// file — the wallpaper model's counterpart of the themes model's `ThemeRenderRecord`
/// (private to the sibling `themes` module, hence named rather than linked), and used for
/// the same reason.
///
/// A hyprlang edit can fail per key (the addressed `wallpaper { }` / `background { }`
/// section does not exist in the user's file, or the chosen image path contains a `#`,
/// which hyprlang would read as an inline comment and silently truncate). The skipped key
/// is logged and the other keys are still written, so [`WallpaperModel::commit`] promotes
/// only the values flagged here: a value that reached no file stays staged, keeping the
/// page dirty and retryable instead of re-baselining the model to something the disk
/// never received.
///
/// Unlike the theme values, these need no separate "a copy refused it" record. With the
/// override off the wallpaper path *is* written to `hyprlock.conf` as well, so it is not
/// quite true that each value lives in one file — but that second copy has its own
/// baseline (`lock.original`), so a copy that refused the value keeps showing up through
/// [`WallpaperModel::hyprlock_write_needed`] and stays retryable. A cursor value has no
/// such per-copy baseline: one model value covers four files, which is why the theme
/// record has to track divergence explicitly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WrittenWallpaperValues {
    /// The wallpaper image path reached the `hyprpaper.conf` write.
    wallpaper: bool,
    /// The fit mode reached the `hyprpaper.conf` write.
    fit: bool,
    /// The lock-screen background path reached the `hyprlock.conf` write.
    lock: bool,
}

impl WrittenWallpaperValues {
    /// Folds one file's render result into the page-wide record.
    fn merge(&mut self, other: WrittenWallpaperValues) {
        self.wallpaper |= other.wallpaper;
        self.fit |= other.fit;
        self.lock |= other.lock;
    }
}

/// The wallpaper / lock-screen background staging model (task 6.5).
///
/// Built by [`WallpaperModel::load`] from the backing [`WallpaperPaths`]. It owns the
/// wallpaper path, the fit-mode [`Selection`], the optional lock-screen override state
/// and its path, the two backing config texts, and a freshness baseline for conflict
/// detection (R5.6). Its file edits reach the shared Apply pipeline through
/// [`Self::apply_contribution`]; the window folds them into the same
/// [`apply::run`](crate::core::apply::run) it drives for the store and the other Theme
/// models.
///
/// # Single wallpaper, optional lock override (analysis §6.2)
///
/// The dotfiles unify the wallpaper and the lock-screen background to the same image,
/// so the UI presents *one* wallpaper path (plus a fit mode) and an optional "use a
/// different lock-screen image" toggle. The path actually written to `hyprlock.conf` is
/// the override path when the toggle is on, otherwise the wallpaper path — so with the
/// toggle off the two files stay in sync automatically.
///
/// # Reload asymmetry (the load-bearing hyprlock rule)
///
/// A change that writes `hyprpaper.conf` reloads live (`hyprctl hyprpaper preload` +
/// `wallpaper`); a change that writes **only** `hyprlock.conf` issues no reload command
/// at all — hyprlock reads its config only at launch, so a lock-background change takes
/// effect at the next lock (intentional, architecture §6). This is enforced by the
/// reload table ([`BackingFile::HyprlockConf`] maps to no action) and by
/// [`Self::reload_params`] setting the wallpaper reload parameter only when
/// `hyprpaper.conf` is written.
///
/// It stays GTK-free so the read, staging, the surgical writes, and the reload decision
/// are unit-tested headlessly (R6.2); the layering guard in
/// `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import.
pub struct WallpaperModel {
    /// `hyprpaper.conf`, or `None` when it was unreadable (R4.4) — the wallpaper rows
    /// are then hidden.
    hyprpaper: Option<BackingText>,
    /// `hyprlock.conf`, or `None` when it was unreadable (R4.4) — the lock override is
    /// then unavailable.
    hyprlock: Option<BackingText>,
    /// The wallpaper image path (`hyprpaper.conf` `wallpaper.path`).
    wallpaper: PathField,
    /// The wallpaper fit mode (`hyprpaper.conf` `wallpaper.fit_mode`).
    fit: Selection,
    /// The lock-screen override image path (`hyprlock.conf` `background.path`). Only
    /// consulted when [`override_on`](Self::override_on) is set.
    lock: PathField,
    /// Whether the lock screen uses a *different* image than the wallpaper (the current
    /// state of the override toggle). Seeded from whether the two on-disk paths already
    /// differ (see [`Self::load`]).
    override_on: bool,
    /// The override toggle's baseline, so [`Self::reset`] restores it and
    /// [`Self::commit`] re-baselines it.
    override_initial: bool,
    /// Whether hyprlock is present at all (its binary was detected). When `false` the
    /// lock override control is hidden and `hyprlock.conf` is never written (R4.2), even
    /// if the file happens to be readable.
    lock_available: bool,
    /// The freshness baseline for the two backing files, recorded from the exact bytes
    /// read at load, so a pre-write conflict check catches an external edit and
    /// [`Self::commit`] re-baselines the app's own write (R5.6). Only readable files are
    /// tracked.
    freshness: FreshnessTracker,
    /// The backing paths, kept so [`Self::reload`] can re-read the files.
    paths: WallpaperPaths,
}

/// The Theme page's wallpaper / lock-background contribution to an
/// [`ApplyPlan`](crate::core::apply::ApplyPlan): the file writes, the reload
/// parameters, and the value validations to re-check (task 6.5).
///
/// A wallpaper-path or fit-mode change contributes a `hyprpaper.conf` write and sets
/// the wallpaper reload parameter; the lock-screen background (the same wallpaper path,
/// or the override) contributes a `hyprlock.conf` write with **no** reload. The
/// validations re-check the chosen image paths at apply time (R8.3), so a path deleted
/// between staging and Apply is caught before any write.
pub struct WallpaperApply {
    /// The atomic writes, one per changed backing file.
    pub writes: Vec<FileWrite>,
    /// The reload parameters — only the wallpaper path, and only when `hyprpaper.conf`
    /// is written (the pipeline merges these into its plan-wide [`ReloadParams`]).
    pub reload_params: ReloadParams,
    /// The chosen image paths to validate before writing (R8.3), reusing the
    /// [`SettingId`] image-path validator.
    pub validations: Vec<(SettingId, Value)>,
}

/// Derives the lock-screen override toggle from a wallpaper path and a lock-screen path:
/// the override is on exactly when the lock screen has its own image that is *not* the
/// wallpaper (analysis §6.2 — the dotfiles unify the two, so "the same image" means no
/// override).
///
/// The single definition of that rule, shared by [`WallpaperModel::load`] (which applies
/// it to the paths read from disk) and [`WallpaperModel::commit`] (which re-applies it to
/// the promoted baselines, i.e. to the paths now on disk). Keeping one copy is what stops
/// the toggle's baseline from disagreeing with the files: a divergence would make
/// [`WallpaperModel::reset`] restore a toggle state the config does not have, turning a
/// clean page into a pending change the user never made (R5.1).
fn override_from_paths(wallpaper: Option<&str>, lock: Option<&str>) -> bool {
    lock.is_some() && lock != wallpaper
}

impl WallpaperModel {
    /// Builds the model by reading the backing config (task 6.5; R4.4, R8.5).
    ///
    /// `paths` is injected (see [`WallpaperPaths`]); `lock_available` is whether the
    /// hyprlock binary was detected — when `false` the lock override is hidden and
    /// `hyprlock.conf` is never written (R4.2). Nothing here fails: an unreadable file
    /// simply yields no backing text (its controls degrade, R4.4). The override toggle
    /// starts on only when the lock-screen path already differs from the wallpaper path
    /// on disk (i.e. the two are not the unified default).
    pub fn load(paths: WallpaperPaths, lock_available: bool) -> WallpaperModel {
        let hyprpaper = read_backing(&paths.hyprpaper_conf);
        let hyprlock = read_backing(&paths.hyprlock_conf);

        let hyprpaper_file = hyprpaper
            .as_ref()
            .map(|backing| HyprlangFile::parse(&backing.text).0);
        let hyprlock_file = hyprlock
            .as_ref()
            .map(|backing| HyprlangFile::parse(&backing.text).0);

        let wallpaper_path = hyprpaper_file
            .as_ref()
            .and_then(|file| file.value(&KeyPath::at(&[WALLPAPER_SECTION], WALLPAPER_PATH_KEY)))
            .map(str::to_string);
        let fit = hyprpaper_file
            .as_ref()
            .and_then(|file| file.value(&KeyPath::at(&[WALLPAPER_SECTION], WALLPAPER_FIT_KEY)))
            .map(str::to_string);
        let lock_path = hyprlock_file
            .as_ref()
            .and_then(|file| file.value(&KeyPath::at(&[LOCK_SECTION], LOCK_PATH_KEY)))
            .map(str::to_string);

        let override_initial = override_from_paths(wallpaper_path.as_deref(), lock_path.as_deref());

        let fit_options: Vec<String> = CURATED_FIT_MODES.iter().map(|s| (*s).to_string()).collect();

        let mut freshness = FreshnessTracker::new();
        for backing in [&hyprpaper, &hyprlock].into_iter().flatten() {
            freshness.record_bytes(backing.path.as_path(), backing.text.as_bytes());
        }

        tracing::info!(
            wallpaper = wallpaper_path.is_some(),
            lock = lock_path.is_some(),
            override_on = override_initial,
            lock_available,
            "loaded wallpaper / lock background for the Theme page (task 6.5)"
        );

        WallpaperModel {
            hyprpaper,
            hyprlock,
            wallpaper: PathField::new(wallpaper_path),
            fit: Selection::new(fit_options, fit),
            lock: PathField::new(lock_path),
            override_on: override_initial,
            override_initial,
            lock_available,
            freshness,
            paths,
        }
    }

    /// Whether the wallpaper rows should be shown: `hyprpaper.conf` was readable, so a
    /// path/fit edit can be written (R4.4).
    pub fn wallpaper_editable(&self) -> bool {
        self.hyprpaper.is_some()
    }

    /// Whether the lock-screen override control should be shown: hyprlock is present and
    /// `hyprlock.conf` is readable, so an override can be written (R4.2/R4.4).
    pub fn lock_editable(&self) -> bool {
        self.lock_available && self.hyprlock.is_some()
    }

    /// The effective wallpaper image path (staged or current), or `None` when unset.
    pub fn wallpaper_path(&self) -> Option<&str> {
        self.wallpaper.effective()
    }

    /// The fit-mode drop-down options (the curated modes plus the current value).
    pub fn fit_options(&self) -> &[String] {
        &self.fit.options
    }

    /// The preselected index of the fit-mode drop-down.
    pub fn selected_fit_index(&self) -> Option<usize> {
        self.fit.selected_index()
    }

    /// Whether the lock-screen override is on (a different image than the wallpaper).
    pub fn override_on(&self) -> bool {
        self.override_on
    }

    /// The effective lock-screen override image path (staged or current), or `None`.
    /// Only meaningful while [`Self::override_on`] is set; the UI shows it in the
    /// override chooser.
    pub fn lock_path(&self) -> Option<&str> {
        self.lock.effective()
    }

    /// Stages a wallpaper image path after validating it (R8.3).
    ///
    /// The path must exist, be readable, and have an image extension; an invalid path is
    /// rejected (returned as an [`Err`] the UI surfaces) and nothing is staged, so a
    /// broken wallpaper can never be written.
    pub fn stage_wallpaper(&mut self, path: &str) -> Result<(), ValidationError> {
        validate_image_path(Path::new(path))?;
        self.wallpaper.stage(path);
        Ok(())
    }

    /// Stages a lock-screen override image path after validating it (R8.3), like
    /// [`Self::stage_wallpaper`].
    pub fn stage_lock(&mut self, path: &str) -> Result<(), ValidationError> {
        validate_image_path(Path::new(path))?;
        self.lock.stage(path);
        Ok(())
    }

    /// Stages a fit mode (a value from the fit-mode drop-down; no path validation).
    pub fn stage_fit(&mut self, fit: &str) {
        self.fit.stage(fit);
    }

    /// Sets the lock-screen override toggle. Turning it off makes the lock screen follow
    /// the wallpaper again; turning it on makes it follow the (separately chosen) lock
    /// path.
    pub fn set_override(&mut self, on: bool) {
        self.override_on = on;
    }

    /// Whether `hyprpaper.conf` needs writing — a wallpaper path or fit-mode change,
    /// and the file is editable.
    fn hyprpaper_write_needed(&self) -> bool {
        self.wallpaper_editable() && (self.wallpaper.is_changed() || self.fit.is_changed())
    }

    /// The image path that should be written to `hyprlock.conf`: the override path when
    /// the override is on, otherwise the wallpaper path (so the two stay in sync).
    fn effective_lock(&self) -> Option<&str> {
        if self.override_on {
            self.lock.effective()
        } else {
            self.wallpaper.effective()
        }
    }

    /// Whether `hyprlock.conf` needs writing — the effective lock path differs from
    /// what is on disk, hyprlock is editable, and there is a path to write.
    fn hyprlock_write_needed(&self) -> bool {
        if !self.lock_editable() {
            return false;
        }
        match self.effective_lock() {
            Some(path) => Some(path) != self.lock.original.as_deref(),
            None => false,
        }
    }

    /// Whether any wallpaper / lock-background value has a pending change — the page's
    /// dirty state, which the window folds into the global Apply/Reset chrome (R5.1).
    pub fn is_dirty(&self) -> bool {
        self.hyprpaper_write_needed() || self.hyprlock_write_needed()
    }

    /// Discards every staged change, returning the toggle to its baseline (R5.1).
    pub fn reset(&mut self) {
        self.wallpaper.reset();
        self.fit.reset();
        self.lock.reset();
        self.override_on = self.override_initial;
    }

    /// Whether either backing file changed on disk since it was loaded (R5.6).
    ///
    /// The Apply glue calls this before writing a dirty change; a `true` result means
    /// another program edited one of the files, so the write must be aborted and the
    /// model reloaded rather than clobbering the stale parse — the same discipline the
    /// Display and GTK/icon/cursor models follow (the pipeline's own conflict check
    /// covers only the store's files, not these bespoke ones).
    pub fn check_conflict(&self) -> bool {
        !self.freshness.check_conflicts().is_empty()
    }

    /// Re-reads the backing files, returning a fresh model with a new freshness baseline
    /// (R5.6 "warn and re-load").
    pub fn reload(&self) -> WallpaperModel {
        WallpaperModel::load(self.paths.clone(), self.lock_available)
    }

    /// The Theme page's wallpaper / lock-background contribution to the Apply plan, or
    /// `None` when nothing changed (task 6.5).
    pub fn apply_contribution(&self) -> Option<WallpaperApply> {
        if !self.is_dirty() {
            return None;
        }
        let (writes, _) = self.render_writes();
        if writes.is_empty() {
            // Dirty but nothing could be written (e.g. a parser edit error). Nothing to
            // apply; the page stays dirty for a retry.
            tracing::warn!(
                "wallpaper change is dirty but no backing file could be written; skipping the wallpaper apply (R4.4)"
            );
            return None;
        }
        Some(WallpaperApply {
            writes,
            reload_params: self.reload_params(),
            validations: self.validations(),
        })
    }

    /// Commits the staged changes after a successful Apply: re-baselines each written
    /// file's freshness from the exact bytes written, updates the in-memory backing
    /// text, and promotes the staged values **that actually reached a file** to their
    /// current value (R5.6).
    ///
    /// Promotion is derived from the rendered writes, not from the
    /// `*_write_needed` predicates: a value whose hyprlang edit was skipped (see
    /// [`WrittenWallpaperValues`]) stays staged, so the page stays dirty and the user can
    /// retry. Promoting it would leave the model claiming a path that reached no file, and
    /// because the value would then no longer look changed, no later Apply would write it
    /// — a permanent silent divergence between the UI and the config.
    pub fn commit(&mut self) {
        let (writes, written) = self.render_writes();
        for write in writes {
            self.freshness
                .record_bytes(write.path.as_path(), &write.contents);
            if let Ok(text) = String::from_utf8(write.contents.clone()) {
                self.set_backing_text(&write.path, text);
            }
        }
        // Capture the effective lock path (which depends on the still-staged wallpaper
        // value when the override is off) before promoting the wallpaper edit.
        let new_lock = self.effective_lock().map(str::to_string);
        if written.wallpaper {
            self.wallpaper.commit();
        }
        if written.fit {
            self.fit.commit();
        }
        if written.lock {
            // `hyprlock.conf` received exactly this path, so it becomes the baseline and
            // the staged override value (if any) has served its purpose.
            self.lock.original = new_lock;
            self.lock.staged = None;
        }
        // Re-derive the toggle's baseline from the promoted baselines — which are now the
        // paths on disk — with the very rule `load` uses. It must not be taken from
        // `override_on`: a `hyprpaper.conf`-only write can change the *relationship*
        // between the two paths (a new wallpaper next to an unchanged lock image) without
        // `hyprlock.conf` being written at all, and a baseline that disagreed with the
        // files would make `reset` restore a toggle state the config does not have —
        // turning a clean page into a pending change the user never made (R5.1).
        self.override_initial = override_from_paths(
            self.wallpaper.original.as_deref(),
            self.lock.original.as_deref(),
        );
    }

    /// Renders the file writes for the current staged changes, together with which values
    /// actually reached one of them (used by both [`Self::apply_contribution`], which
    /// needs only the writes, and [`Self::commit`], which needs the promotion record).
    fn render_writes(&self) -> (Vec<FileWrite>, WrittenWallpaperValues) {
        let mut writes = Vec::new();
        let mut written = WrittenWallpaperValues::default();
        if self.hyprpaper_write_needed() {
            if let Some(backing) = &self.hyprpaper {
                let (write, keys) = self.render_hyprpaper(backing);
                written.merge(keys);
                writes.extend(write);
            }
        }
        if self.hyprlock_write_needed() {
            if let Some(backing) = &self.hyprlock {
                if let Some(write) = self.render_hyprlock(backing) {
                    written.lock = true;
                    writes.push(write);
                }
            }
        }
        (writes, written)
    }

    /// Renders the `hyprpaper.conf` write, editing only the `wallpaper.path` and/or
    /// `wallpaper.fit_mode` value spans that changed (surgical, R5.3), and reports which
    /// of the two the edit actually landed for.
    fn render_hyprpaper(
        &self,
        backing: &BackingText,
    ) -> (Option<FileWrite>, WrittenWallpaperValues) {
        let (mut file, _) = HyprlangFile::parse(&backing.text);
        let mut changed_keys = Vec::new();
        let mut written = WrittenWallpaperValues::default();

        if self.wallpaper.is_changed() {
            if let Some(path) = self.wallpaper.effective() {
                match file.set_value(&KeyPath::at(&[WALLPAPER_SECTION], WALLPAPER_PATH_KEY), path) {
                    Ok(()) => {
                        changed_keys.push("wallpaper path".to_string());
                        written.wallpaper = true;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not set the wallpaper path in hyprpaper.conf");
                    }
                }
            }
        }
        if self.fit.is_changed() {
            if let Some(fit) = self.fit.effective() {
                match file.set_value(&KeyPath::at(&[WALLPAPER_SECTION], WALLPAPER_FIT_KEY), fit) {
                    Ok(()) => {
                        changed_keys.push("fit mode".to_string());
                        written.fit = true;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not set the fit mode in hyprpaper.conf");
                    }
                }
            }
        }

        if changed_keys.is_empty() {
            return (None, written);
        }
        (
            Some(FileWrite {
                path: backing.path.clone(),
                contents: file.emit().into_bytes(),
                changed_keys,
                backing: BackingFile::HyprpaperConf,
                // Task 9.10: this write carries a validated value only when it carries
                // the wallpaper *path* — [`Self::validations`] re-checks that path (it
                // must still exist and be readable, R8.3) under the same condition that
                // put it in this write. A fit-only change carries just a mode from a
                // fixed list, and deliberately does not drag the unchanged path into
                // validation, so such a write is validation-free by design.
                validation: if written.wallpaper {
                    WriteValidation::InPlan
                } else {
                    WriteValidation::NotNeeded
                },
            }),
            written,
        )
    }

    /// Renders the `hyprlock.conf` write, editing only the `background.path` value span
    /// to the effective lock path (surgical, R5.3).
    ///
    /// `None` means the edit could not be made — most plausibly a `hyprlock.conf` with no
    /// `background { }` section, which the hyprlang writer never invents — and is what
    /// keeps [`Self::commit`] from promoting the lock path (see
    /// [`WrittenWallpaperValues`]).
    fn render_hyprlock(&self, backing: &BackingText) -> Option<FileWrite> {
        let path = self.effective_lock()?;
        let (mut file, _) = HyprlangFile::parse(&backing.text);
        match file.set_value(&KeyPath::at(&[LOCK_SECTION], LOCK_PATH_KEY), path) {
            Ok(()) => Some(FileWrite {
                path: backing.path.clone(),
                contents: file.emit().into_bytes(),
                changed_keys: vec!["lock background path".to_string()],
                backing: BackingFile::HyprlockConf,
                // This write exists only when there is an effective lock path, which is
                // exactly the condition under which [`Self::validations`] re-checks that
                // path (R8.3) — so its value is always covered by the plan.
                validation: WriteValidation::InPlan,
            }),
            Err(error) => {
                tracing::warn!(%error, "could not set the lock background path in hyprlock.conf");
                None
            }
        }
    }

    /// The reload parameters for the change (task 4.4).
    ///
    /// The wallpaper path **and** the fit mode are set only when `hyprpaper.conf` is
    /// written, so the live `hyprctl hyprpaper wallpaper <monitor>,<path>,<fit>` applies
    /// the current fit (a fit-only change re-sets the same image with the new fit). A
    /// hyprlock-only change carries no wallpaper parameter — and, combined with
    /// [`BackingFile::HyprlockConf`] mapping to no action, issues no reload command at
    /// all (the intentional hyprlock behaviour, architecture §6).
    fn reload_params(&self) -> ReloadParams {
        let (wallpaper, fit) = if self.hyprpaper_write_needed() {
            (
                self.wallpaper.effective().map(str::to_string),
                self.fit.effective().map(str::to_string),
            )
        } else {
            (None, None)
        };
        ReloadParams {
            wallpaper,
            fit,
            cursor: None,
            gtk_theme: None,
            icon_theme: None,
        }
    }

    /// The chosen image paths to validate before writing (R8.3): the wallpaper path only
    /// when it is actually being (re)written (a fit-only change never rewrites — nor
    /// should it re-validate — the unchanged wallpaper path), and the effective lock
    /// path when `hyprlock.conf` is written.
    fn validations(&self) -> Vec<(SettingId, Value)> {
        let mut validations = Vec::new();
        if self.wallpaper.is_changed() {
            if let Some(path) = self.wallpaper.effective() {
                validations.push((SettingId::WallpaperPath, Value::String(path.to_string())));
            }
        }
        if self.hyprlock_write_needed() {
            if let Some(path) = self.effective_lock() {
                validations.push((
                    SettingId::LockBackgroundPath,
                    Value::String(path.to_string()),
                ));
            }
        }
        validations
    }

    /// Updates the in-memory backing text for the file at `path` after a commit, so a
    /// subsequent edit re-parses the current bytes.
    fn set_backing_text(&mut self, path: &Path, text: String) {
        for backing in [self.hyprpaper.as_mut(), self.hyprlock.as_mut()]
            .into_iter()
            .flatten()
        {
            if backing.path == path {
                backing.text = text;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::core::apply::{self, ApplyOutcome, ApplyPlan};
    use crate::core::detect::{Binary, Capabilities, Daemon};
    use crate::core::freshness::FreshnessTracker;
    use crate::system::command::{Command, MockCommandRunner};
    use crate::system::signal::MockProcessSignaller;

    /// A `hyprpaper.conf` fixture: a `wallpaper { }` block with a monitor line, a
    /// comment, the wallpaper `path`, and `fit_mode`, plus a top-level `splash` key —
    /// the shape from the hyprlang parser fixtures (analysis §6.2).
    const HYPRPAPER_CONF: &str = "\
wallpaper {
    monitor =
    # Keep in sync with hyprlock.conf's background.path (same image).
    path = ~/Pictures/wallpaper/18.jpg
    fit_mode = cover
}

splash = false
";

    /// A `hyprlock.conf` fixture: a `source =`, a `background { }` block with the
    /// lock-screen `path` and an inline comment, and an unrelated `label { }` block.
    const HYPRLOCK_CONF: &str = "\
source = ~/.config/hypr/colors.conf

background {
    monitor =
    path = ~/Pictures/wallpaper/18.jpg
    blur_passes = 2 # 0 disables blurring
}

label {
    text = Hi
}
";

    /// Writes the two backing files into `dir/hypr` and returns their
    /// [`WallpaperPaths`].
    fn write_wallpaper_fixture(dir: &Path) -> WallpaperPaths {
        let hypr = dir.join("hypr");
        fs::create_dir_all(&hypr).expect("create hypr config subdir");
        let hyprpaper_conf = hypr.join("hyprpaper.conf");
        let hyprlock_conf = hypr.join("hyprlock.conf");
        fs::write(&hyprpaper_conf, HYPRPAPER_CONF).expect("write hyprpaper.conf");
        fs::write(&hyprlock_conf, HYPRLOCK_CONF).expect("write hyprlock.conf");
        WallpaperPaths {
            hyprpaper_conf,
            hyprlock_conf,
        }
    }

    /// Writes a real (present, readable) file with an image extension at `dir/name` and
    /// returns its path string, so `validate_image_path` accepts it (it checks the
    /// extension and readability, not the bytes — task 4.1).
    fn write_image(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, b"img").expect("write image file");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_wallpaper_and_fit_change_writes_both_files_and_reloads_hyprpaper() {
        // Accept criterion: a staged wallpaper path + fit-mode edit produces the exact
        // surgical hyprpaper.conf/hyprlock.conf FileWrite diffs AND the hyprctl hyprpaper
        // preload/wallpaper command sequence. With no override, the same path is written
        // to both files (the single-wallpaper UX, analysis §6.2).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let wall = write_image(tmp.path(), "wall.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        assert!(
            !model.override_on(),
            "the unified same-image default means no override"
        );
        model
            .stage_wallpaper(&wall)
            .expect("a real image validates");
        model.stage_fit("contain");
        assert!(model.is_dirty());

        let contribution = model.apply_contribution().expect("a change contributes");
        assert_eq!(
            contribution.writes.len(),
            2,
            "both hyprpaper.conf and hyprlock.conf are written (same path to both)"
        );
        // The chosen paths are re-validated at apply time (R8.3), and both writes say so
        // (task 9.10) — the counterpart to the fit-only case below, where the same
        // hyprpaper.conf write is validation-free.
        assert_eq!(
            contribution.validations,
            vec![
                (SettingId::WallpaperPath, Value::String(wall.clone())),
                (SettingId::LockBackgroundPath, Value::String(wall.clone())),
            ]
        );
        assert!(
            contribution
                .writes
                .iter()
                .all(|write| write.validation == WriteValidation::InPlan),
            "a path-carrying write's values are validated by the plan"
        );

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the wallpaper apply must succeed, got {outcome:?}"
        );

        // hyprpaper.conf: ONLY the path and fit_mode value spans changed — building the
        // expected text by replacing exactly those two spans proves every other byte
        // (the comment, the `monitor =` line, `splash`) is untouched.
        let expected_hyprpaper = HYPRPAPER_CONF
            .replace("~/Pictures/wallpaper/18.jpg", &wall)
            .replace("fit_mode = cover", "fit_mode = contain");
        assert_eq!(
            fs::read_to_string(&paths.hyprpaper_conf).unwrap(),
            expected_hyprpaper
        );
        // hyprlock.conf: ONLY background.path changed, to the SAME wallpaper path.
        let expected_hyprlock = HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &wall);
        assert_eq!(
            fs::read_to_string(&paths.hyprlock_conf).unwrap(),
            expected_hyprlock
        );

        // The reload: preload the image, then set it on all outputs (empty monitor
        // field before the comma) with the staged fit as the third comma-field, so the
        // fit is applied live (task 6.5). No hyprlock reload (see the dedicated test).
        let set_arg = format!(",{wall},contain");
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl").args(["hyprpaper", "preload", wall.as_str()]),
                Command::new("hyprctl").args(["hyprpaper", "wallpaper", set_arg.as_str()]),
            ]
        );
    }

    #[test]
    fn a_fit_only_change_reloads_hyprpaper_with_the_new_fit() {
        // MAJOR-fix (task 6.5): a fit-only change still reloads hyprpaper — it re-sets
        // the (unchanged) wallpaper image with the NEW fit as the third comma-field, so
        // the fit takes effect live rather than only on the next hyprpaper restart.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);

        let mut model = WallpaperModel::load(paths, true);
        // Change ONLY the fit; the wallpaper path stays the on-disk value.
        model.stage_fit("tile");
        assert!(model.is_dirty());

        let contribution = model
            .apply_contribution()
            .expect("a fit change contributes");
        // Only hyprpaper.conf is written (the lock path is unchanged), and the reload
        // carries the current wallpaper path AND the new fit. The unchanged wallpaper
        // path is not re-validated (a fit change should not fail on it, R8.3).
        assert_eq!(
            contribution.writes.len(),
            1,
            "only hyprpaper.conf is written"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprpaperConf);
        assert!(
            contribution.validations.is_empty(),
            "a fit-only change re-validates no path (the wallpaper is unchanged)"
        );
        // Task 9.10: because this legitimate apply has writes and no validations, the
        // write must declare itself validation-free — otherwise the Apply pipeline's
        // plan-drift guard would warn "the caller dropped its validations" on every
        // fit-only change.
        assert_eq!(
            contribution.writes[0].validation,
            WriteValidation::NotNeeded,
            "a fit-only write carries nothing to validate and must say so"
        );
        assert_eq!(
            contribution.reload_params.wallpaper.as_deref(),
            Some("~/Pictures/wallpaper/18.jpg")
        );
        assert_eq!(contribution.reload_params.fit.as_deref(), Some("tile"));

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let runner = MockCommandRunner::new();
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        let outcome = apply::run(
            &plan,
            &FreshnessTracker::new(),
            &caps,
            &runner,
            &MockProcessSignaller::new(),
        );
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the fit-only apply must succeed, got {outcome:?}"
        );
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl").args([
                    "hyprpaper",
                    "preload",
                    "~/Pictures/wallpaper/18.jpg"
                ]),
                Command::new("hyprctl").args([
                    "hyprpaper",
                    "wallpaper",
                    ",~/Pictures/wallpaper/18.jpg,tile"
                ]),
            ]
        );
    }

    #[test]
    fn a_lock_override_only_change_writes_hyprlock_with_no_reload() {
        // Accept criterion: a change that touches ONLY hyprlock.conf (the lock-screen
        // override) issues NO reload command — hyprlock reads its config at the next
        // lock (intentional, architecture §6).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let lock_image = write_image(tmp.path(), "lock.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        // Turn on the override and pick a different lock image; leave the wallpaper and
        // fit unchanged, so only hyprlock.conf is written.
        model.set_override(true);
        model
            .stage_lock(&lock_image)
            .expect("a real image validates");
        assert!(model.is_dirty());

        let contribution = model.apply_contribution().expect("a change contributes");
        assert_eq!(
            contribution.writes.len(),
            1,
            "only hyprlock.conf is written for a lock-override-only change"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprlockConf);
        assert!(
            contribution.reload_params.wallpaper.is_none(),
            "a hyprlock-only change carries no wallpaper reload parameter"
        );

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let tracker = FreshnessTracker::new();
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();
        // hyprpaper is fully live, yet a hyprlock-only change still plans no reload.
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        let outcome = apply::run(&plan, &tracker, &caps, &runner, &signaller);
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "the lock apply must succeed, got {outcome:?}"
        );
        assert!(
            runner.recorded().is_empty(),
            "a hyprlock-only change must issue no reload command"
        );

        // hyprpaper.conf is untouched; only hyprlock's background.path changed.
        assert_eq!(
            fs::read_to_string(&paths.hyprpaper_conf).unwrap(),
            HYPRPAPER_CONF
        );
        let expected_hyprlock = HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &lock_image);
        assert_eq!(
            fs::read_to_string(&paths.hyprlock_conf).unwrap(),
            expected_hyprlock
        );
    }

    #[test]
    fn staging_rejects_a_non_existent_or_non_image_path() {
        // R8.3: a chosen path is validated (exists + readable + image extension) before
        // staging; an invalid path is rejected and nothing is staged.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let mut model = WallpaperModel::load(paths, true);

        // A path that does not exist.
        let missing = tmp.path().join("no-such.png");
        assert!(
            model.stage_wallpaper(&missing.to_string_lossy()).is_err(),
            "a non-existent path is rejected"
        );
        // A real file with a non-image extension.
        let text = tmp.path().join("notes.txt");
        fs::write(&text, b"hi").unwrap();
        assert!(
            model.stage_wallpaper(&text.to_string_lossy()).is_err(),
            "a non-image file is rejected"
        );
        // The same guard applies to the lock override path.
        assert!(
            model.stage_lock(&missing.to_string_lossy()).is_err(),
            "the lock override path is validated too"
        );
        assert!(!model.is_dirty(), "a rejected path is never staged");
    }

    #[test]
    fn controls_hide_when_hyprlock_is_absent_or_config_is_unreadable() {
        // R4.2/R4.4: the lock-override control is hidden when hyprlock is absent, and the
        // wallpaper rows are hidden when hyprpaper.conf is unreadable.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);

        // hyprlock absent: the wallpaper is editable, but the lock override is not.
        let mut no_lock = WallpaperModel::load(paths.clone(), false);
        assert!(no_lock.wallpaper_editable());
        assert!(
            !no_lock.lock_editable(),
            "no hyprlock -> no lock-override control (R4.2)"
        );
        // A wallpaper change then writes ONLY hyprpaper.conf — hyprlock.conf is left
        // alone when the lock is not editable.
        let wall = write_image(tmp.path(), "wall.png");
        no_lock.stage_wallpaper(&wall).unwrap();
        let contribution = no_lock
            .apply_contribution()
            .expect("a wallpaper change contributes");
        assert_eq!(
            contribution.writes.len(),
            1,
            "only hyprpaper.conf is written"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprpaperConf);

        // hyprpaper.conf unreadable: the wallpaper rows hide (R4.4).
        let unreadable = WallpaperModel::load(
            WallpaperPaths {
                hyprpaper_conf: config.join("hypr").join("does-not-exist.conf"),
                hyprlock_conf: paths.hyprlock_conf.clone(),
            },
            true,
        );
        assert!(
            !unreadable.wallpaper_editable(),
            "an unreadable hyprpaper.conf hides the wallpaper rows (R4.4)"
        );
    }

    #[test]
    fn reset_restores_the_override_toggle_and_commit_is_not_a_self_conflict() {
        // Reset restores the toggle baseline and clears staged paths (R5.1); and after a
        // committed apply the app's own write is not seen as an external conflict (R5.6).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let wall = write_image(tmp.path(), "wall.png");
        let lock_image = write_image(tmp.path(), "lock.png");

        let mut model = WallpaperModel::load(paths, true);
        // Toggle the override on and stage a distinct lock image, then reset.
        model.set_override(true);
        model.stage_lock(&lock_image).unwrap();
        assert!(model.is_dirty());
        model.reset();
        assert!(!model.is_dirty(), "reset discards the pending override");
        assert!(
            !model.override_on(),
            "reset restores the toggle baseline (off)"
        );

        // Now stage a wallpaper change, apply it, and commit.
        model.stage_wallpaper(&wall).unwrap();
        let contribution = model.apply_contribution().expect("a change contributes");
        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        assert!(matches!(
            apply::run(
                &plan,
                &FreshnessTracker::new(),
                &caps,
                &MockCommandRunner::new(),
                &MockProcessSignaller::new(),
            ),
            ApplyOutcome::Applied { .. }
        ));
        assert!(
            model.check_conflict(),
            "before commit the write differs from the load-time baseline"
        );
        model.commit();
        assert!(!model.is_dirty(), "commit clears the dirty state");
        assert!(
            !model.check_conflict(),
            "after commit the app's own write is not a self-conflict (R5.6)"
        );
    }

    /// Writes a fixture whose `hyprpaper.conf` and `hyprlock.conf` start with two
    /// **different** on-disk image paths (both real files, so validation accepts them),
    /// returning the paths plus the wallpaper (`A`) and lock (`B`) path strings. This
    /// exercises the override-on branches that the unified same-path fixture cannot.
    fn write_distinct_wallpaper_fixture(
        root: &Path,
        config: &Path,
    ) -> (WallpaperPaths, String, String) {
        let wallpaper_a = write_image(root, "wall_a.png");
        let lock_b = write_image(root, "lock_b.png");
        let hypr = config.join("hypr");
        fs::create_dir_all(&hypr).expect("create hypr config subdir");
        let hyprpaper_conf = hypr.join("hyprpaper.conf");
        let hyprlock_conf = hypr.join("hyprlock.conf");
        fs::write(
            &hyprpaper_conf,
            HYPRPAPER_CONF.replace("~/Pictures/wallpaper/18.jpg", &wallpaper_a),
        )
        .expect("write hyprpaper.conf");
        fs::write(
            &hyprlock_conf,
            HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &lock_b),
        )
        .expect("write hyprlock.conf");
        (
            WallpaperPaths {
                hyprpaper_conf,
                hyprlock_conf,
            },
            wallpaper_a,
            lock_b,
        )
    }

    #[test]
    fn distinct_on_disk_paths_start_with_the_override_on() {
        // When the two on-disk paths differ (not the unified default), the lock-screen
        // override toggle starts on, preselecting each file's own path.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let (paths, wallpaper_a, lock_b) = write_distinct_wallpaper_fixture(tmp.path(), &config);

        let model = WallpaperModel::load(paths, true);
        assert!(
            model.override_on(),
            "distinct on-disk paths -> the override starts on"
        );
        assert_eq!(model.wallpaper_path(), Some(wallpaper_a.as_str()));
        assert_eq!(model.lock_path(), Some(lock_b.as_str()));
        assert!(!model.is_dirty(), "no edit is staged at load");
    }

    #[test]
    fn a_distinct_lock_override_writes_both_files_to_their_two_paths() {
        // With the override on, changing both the wallpaper and the lock image writes
        // each file to its own (different) path.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let (paths, _wallpaper_a, _lock_b) = write_distinct_wallpaper_fixture(tmp.path(), &config);
        let wallpaper_c = write_image(tmp.path(), "wall_c.png");
        let lock_d = write_image(tmp.path(), "lock_d.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        assert!(model.override_on());
        model
            .stage_wallpaper(&wallpaper_c)
            .expect("a real image validates");
        model.stage_lock(&lock_d).expect("a real image validates");

        let contribution = model.apply_contribution().expect("a change contributes");
        assert_eq!(
            contribution.writes.len(),
            2,
            "both files are written, to their two different paths"
        );

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        assert!(matches!(
            apply::run(
                &plan,
                &FreshnessTracker::new(),
                &caps,
                &MockCommandRunner::new(),
                &MockProcessSignaller::new(),
            ),
            ApplyOutcome::Applied { .. }
        ));

        assert!(
            fs::read_to_string(&paths.hyprpaper_conf)
                .unwrap()
                .contains(&format!("path = {wallpaper_c}")),
            "hyprpaper.conf points at the new wallpaper"
        );
        assert!(
            fs::read_to_string(&paths.hyprlock_conf)
                .unwrap()
                .contains(&format!("path = {lock_d}")),
            "hyprlock.conf points at the distinct override image"
        );
    }

    #[test]
    fn turning_the_override_off_resyncs_the_lock_to_the_wallpaper_with_no_reload() {
        // Turning the override off resyncs the lock-screen background to the wallpaper:
        // a single hyprlock.conf write equal to the wallpaper path, with no reload
        // (hyprlock reads its config at the next lock).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let (paths, wallpaper_a, _lock_b) = write_distinct_wallpaper_fixture(tmp.path(), &config);

        let mut model = WallpaperModel::load(paths, true);
        assert!(model.override_on());
        model.set_override(false);
        assert!(
            model.is_dirty(),
            "resyncing the lock back to the wallpaper is a pending change"
        );

        let contribution = model.apply_contribution().expect("the resync contributes");
        assert_eq!(
            contribution.writes.len(),
            1,
            "only hyprlock.conf is written on a resync"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprlockConf);
        assert!(
            contribution.reload_params.wallpaper.is_none(),
            "a hyprlock-only resync carries no wallpaper reload parameter -> no reload"
        );
        let text = String::from_utf8(contribution.writes[0].contents.clone()).unwrap();
        assert!(
            text.contains(&format!("path = {wallpaper_a}")),
            "the lock background is resynced to the wallpaper path"
        );
    }

    #[test]
    fn an_unusual_on_disk_fit_mode_stays_selectable() {
        // NIT (task 6.5): a non-curated on-disk fit_mode value is kept as the preselected
        // option (Selection::new prepends it), so an unusual value is never dropped.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        fs::write(
            &paths.hyprpaper_conf,
            HYPRPAPER_CONF.replace("fit_mode = cover", "fit_mode = stretch"),
        )
        .expect("write an unusual fit_mode");

        let model = WallpaperModel::load(paths, true);
        let selected = model
            .selected_fit_index()
            .and_then(|index| model.fit_options().get(index))
            .map(String::as_str);
        assert_eq!(
            selected,
            Some("stretch"),
            "an unusual on-disk fit_mode stays the preselected option"
        );
        assert!(
            model.fit_options().contains(&"stretch".to_string()),
            "the unusual value is present among the options"
        );
    }

    #[test]
    fn a_skipped_hyprlock_write_leaves_the_lock_path_un_promoted() {
        // Task 9.6 accept criterion: a skipped hyprlock write leaves the lock field
        // un-promoted. A `hyprlock.conf` without a `background { }` block cannot receive
        // the lock path — the hyprlang writer never invents a section — while the
        // wallpaper's own `hyprpaper.conf` write succeeds and is applied. Commit must
        // promote the wallpaper and leave the lock baseline alone: claiming the lock path
        // had been written would clear the pending write for good, because the model would
        // then see the file as already holding it.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        fs::write(
            &paths.hyprlock_conf,
            "source = ~/.config/hypr/colors.conf\n\nlabel {\n    text = Hi\n}\n",
        )
        .expect("write a hyprlock.conf with no background section");
        let hyprlock_before =
            fs::read_to_string(&paths.hyprlock_conf).expect("read the hyprlock fixture");
        let wall = write_image(tmp.path(), "wall.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        model
            .stage_wallpaper(&wall)
            .expect("a real image validates");

        let contribution = model
            .apply_contribution()
            .expect("the wallpaper change contributes");
        assert_eq!(
            contribution.writes.len(),
            1,
            "only hyprpaper.conf could be rendered"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprpaperConf);

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        assert!(matches!(
            apply::run(
                &plan,
                &FreshnessTracker::new(),
                &caps,
                &MockCommandRunner::new(),
                &MockProcessSignaller::new(),
            ),
            ApplyOutcome::Applied { .. }
        ));

        model.commit();
        assert_eq!(
            fs::read_to_string(&paths.hyprlock_conf).expect("read hyprlock.conf"),
            hyprlock_before,
            "hyprlock.conf is byte-identical: it never received the path"
        );
        assert_eq!(
            model.lock.original, None,
            "the lock baseline is not promoted to a path the file never received"
        );
        assert!(
            model.is_dirty(),
            "the pending lock write survives the commit, so a retry can still write it"
        );
        assert_eq!(
            model.wallpaper_path(),
            Some(wall.as_str()),
            "the wallpaper path, which was written, is promoted"
        );
    }

    #[test]
    fn a_rejected_wallpaper_path_stays_dirty_while_the_fit_mode_is_promoted() {
        // Task 9.6, per-key promotion *within* one file: an image whose filename contains
        // a `#` is a real, readable file (so `stage_wallpaper`'s validation accepts it) but
        // the hyprlang writer refuses to write it, because hyprlang would read the `#` as
        // the start of an inline comment and silently truncate the path. The fit mode
        // staged in the same file is written regardless, so commit promotes the fit and
        // leaves the wallpaper path staged.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let hashed = write_image(tmp.path(), "wall#1.png");

        let mut model = WallpaperModel::load(paths, true);
        model
            .stage_wallpaper(&hashed)
            .expect("a real image file validates");
        model.stage_fit("tile");

        let contribution = model
            .apply_contribution()
            .expect("the fit-mode change still contributes");
        assert_eq!(
            contribution.writes.len(),
            1,
            "hyprpaper.conf carries the fit mode; the lock write is rejected for the same reason"
        );
        assert_eq!(
            contribution.writes[0].changed_keys,
            vec!["fit mode".to_string()],
            "the rejected wallpaper path is skipped"
        );
        let rendered = String::from_utf8(contribution.writes[0].contents.clone())
            .expect("hyprpaper.conf stays UTF-8");
        assert!(
            rendered.contains("fit_mode = tile")
                && rendered.contains("path = ~/Pictures/wallpaper/18.jpg"),
            "the fit mode is rewritten while the wallpaper path keeps its on-disk value"
        );

        model.commit();
        let selected_fit = model
            .selected_fit_index()
            .and_then(|index| model.fit_options().get(index))
            .map(String::as_str);
        assert_eq!(
            selected_fit,
            Some("tile"),
            "the fit mode reached the file, so it is promoted"
        );
        assert_eq!(
            model.wallpaper.original.as_deref(),
            Some("~/Pictures/wallpaper/18.jpg"),
            "the wallpaper baseline still matches what is on disk"
        );
        assert!(
            model.wallpaper.is_changed() && model.is_dirty(),
            "the rejected wallpaper path stays staged and the page stays dirty"
        );
    }

    #[test]
    fn a_hyprpaper_only_write_rebaselines_the_override_toggle_from_the_new_paths() {
        // R5.1 / task 9.6: the override toggle's baseline describes the *relationship*
        // between the two on-disk paths, and a `hyprpaper.conf`-only write can change that
        // relationship without `hyprlock.conf` being written at all. On a unified host
        // (both files point at the same image) the user can turn the override on without
        // picking a lock image — nothing needs writing to hyprlock.conf, because the path
        // the override would keep is the one already there — and then change the wallpaper.
        // The two paths now differ, so the toggle's baseline must be "on", exactly as a
        // fresh load of the same files derives it. A baseline left at "off" would make
        // Reset turn the override off, inventing a pending change the user never made
        // (which, if applied, would silently resync the lock background to the wallpaper).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let wall = write_image(tmp.path(), "new_wall.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        assert!(
            !model.override_on(),
            "the unified fixture (both files on the same image) starts with no override"
        );
        model.set_override(true);
        model
            .stage_wallpaper(&wall)
            .expect("a real image validates");

        let contribution = model
            .apply_contribution()
            .expect("the wallpaper change contributes");
        assert_eq!(
            contribution.writes.len(),
            1,
            "hyprlock.conf already holds the path the override keeps, so only \
             hyprpaper.conf is written"
        );
        assert_eq!(contribution.writes[0].backing, BackingFile::HyprpaperConf);

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            palette: None,
            reload_params: contribution.reload_params,
        };
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true);
        assert!(matches!(
            apply::run(
                &plan,
                &FreshnessTracker::new(),
                &caps,
                &MockCommandRunner::new(),
                &MockProcessSignaller::new(),
            ),
            ApplyOutcome::Applied { .. }
        ));

        model.commit();
        assert!(!model.is_dirty(), "the applied change leaves a clean page");
        assert!(model.override_on(), "the override the user set stays on");

        // The load-bearing assertion: Reset must be a no-op on a clean page.
        model.reset();
        assert!(
            model.override_on(),
            "Reset keeps the override on, because that is what the two files now say"
        );
        assert!(
            !model.is_dirty(),
            "Reset on a clean page creates no pending change"
        );

        // Cross-check against the ground truth: a fresh read of the same files derives the
        // same toggle state, so the model and the config agree.
        let reloaded = WallpaperModel::load(paths, true);
        assert!(
            reloaded.override_on(),
            "a fresh load of the written files also derives the override as on"
        );
        assert_eq!(
            reloaded.wallpaper_path(),
            Some(wall.as_str()),
            "the wallpaper on disk is the new image"
        );
        assert_eq!(
            reloaded.lock_path(),
            Some("~/Pictures/wallpaper/18.jpg"),
            "the lock background on disk is untouched, which is why the two now differ"
        );
    }
}
