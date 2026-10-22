//! The wallpaper / lock-screen background staging model for the Theme page (task 6.5;
//! R4.2, R4.4, R5.6, R8.3) — see [`super`] for how the page's three models fit together.
//!
//! The desktop wallpaper (`hyprpaper.conf`) and the lock-screen background
//! (`hyprlock.conf`) both default to the *same* image in the dotfiles (analysis §6.2), so
//! the page presents one wallpaper path plus a fit-mode drop-down and an optional "use a
//! different lock-screen image" override: with no override the same path is written to
//! both files; with one, the wallpaper goes to `hyprpaper.conf` and the override to
//! `hyprlock.conf`. A wallpaper change reloads live via one `hyprctl hyprpaper
//! wallpaper` request (plus the eww bar's text-tone follow-up where present, task
//! 11.14), while a hyprlock-only change issues **no** reload (hyprlock
//! reads its config only at the next lock — intentional, architecture §6). Chosen image
//! paths are validated (exists + readable + image extension, and holdable by every file
//! and request they are written to — task 9.27; R8.3) before staging.
//! Both files may store their path home-relative (`~/Pictures/…`, analysis §7.4); how
//! the model validates, previews, writes and reloads such a value is described under
//! "Home-relative paths" on [`WallpaperModel`], whose docs carry the rest.

use std::path::{Path, PathBuf};

use crate::core::apply::{FileWrite, WriteMode};
use crate::core::home_path::{expand_home, expand_home_to_string, path_in_form_of};
use crate::core::model::{
    SettingId, ValidationError, Value, validate_lock_background_value, validate_wallpaper_value,
};
use crate::core::reload::{BackingFile, ReloadParams};
use crate::parsers::hyprlang::{HyprlangFile, KeyPath};

use super::{BackingSet, BackingText, Selection, Staged};

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

/// The fit mode the config file accepts but a live wallpaper change cannot show (see
/// [`WallpaperModel::fit_preview_note`]).
const FILL_FIT_MODE: &str = "fill";

/// The live XDG paths of the two config files a wallpaper / lock-background change
/// writes (R8.5), plus the home directory their `~/…` image paths are relative to.
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
    /// The home directory a `~/…` image path in either file expands against
    /// ([`crate::core::home_path::home_dir`] in production, the fixture's fake home in
    /// tests), or `None` when it is unknown — a home-relative path then fails
    /// validation instead of being guessed (task 11.10).
    pub home: Option<PathBuf>,
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
/// A change that writes `hyprpaper.conf` reloads live (one `hyprctl hyprpaper
/// wallpaper` request, then the eww bar's text-tone script when it exists — see
/// [`ReloadAction::HyprpaperWallpaper`](crate::core::reload::ReloadAction::HyprpaperWallpaper));
/// a change that writes **only** `hyprlock.conf` issues no reload command
/// at all — hyprlock reads its config only at launch, so a lock-background change takes
/// effect at the next lock (intentional, architecture §6). This is enforced by the
/// reload table ([`BackingFile::HyprlockConf`] maps to no action) and by
/// [`Self::reload_params`] setting the wallpaper reload parameter only when
/// `hyprpaper.conf` is written.
///
/// # Home-relative paths (task 11.10)
///
/// The dotfiles store both paths as `~/Pictures/wallpaper/18.jpg` (analysis §7.4) so the
/// repo stays portable. The model keeps every path **in the form its file uses** — the
/// staged and baseline values are exactly the text that is (or will be) in the file, so
/// the dirty comparison stays a plain string comparison and re-choosing the configured
/// image leaves the page clean. The rules (all in [`crate::core::home_path`]):
///
/// - a chosen image below the home directory is staged as `~/…` when the target file's
///   current value is home-relative, and absolute otherwise; an image outside the home
///   directory is always absolute. `hyprpaper.conf` and `hyprlock.conf` decide
///   independently from their own current value;
/// - validation (R8.3) and the file chooser's starting point ([`Self::wallpaper_file`],
///   [`Self::lock_file`]) use the path expanded against [`WallpaperPaths::home`];
/// - the hyprpaper reload is given the expanded absolute path: hyprpaper's IPC server
///   rejects any path not starting with `/` (hyprpaper 0.8.4, `CWallpaperObject::apply`
///   in `src/ipc/IPC.cpp`). The `hyprctl hyprpaper` client at Hyprland tag v0.56.2
///   (`getFullPath` in `hyprctl/src/hyprpaper/Hyprpaper.cpp`) happens to
///   expand a leading `~` before sending, but relying on that client-side detail is
///   unnecessary when the app knows the absolute path.
///
/// It stays GTK-free so the read, staging, the surgical writes, and the reload decision
/// are unit-tested headlessly (R6.2); the layering guard in
/// `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import.
pub struct WallpaperModel {
    /// The two backing config files this model edits, with their freshness baseline
    /// (R5.6). Reached through the per-file accessors [`Self::hyprpaper`] and
    /// [`Self::hyprlock`].
    backing: BackingSet,
    /// The wallpaper image path (`hyprpaper.conf` `wallpaper.path`).
    wallpaper: Staged,
    /// The wallpaper fit mode (`hyprpaper.conf` `wallpaper.fit_mode`).
    fit: Selection,
    /// The lock-screen override image path (`hyprlock.conf` `background.path`). Only
    /// consulted when [`override_on`](Self::override_on) is set.
    lock: Staged,
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
/// "The same image" is decided on the paths expanded against `home` (task 11.10): each
/// file keeps its own form, so `~/Pictures/a.jpg` in one and `/home/me/Pictures/a.jpg`
/// in the other still name one image and must not switch the override on.
///
/// The single definition of that rule, shared by [`WallpaperModel::load`] (which applies
/// it to the paths read from disk) and [`WallpaperModel::commit`] (which re-applies it to
/// the promoted baselines, i.e. to the paths now on disk). Keeping one copy is what stops
/// the toggle's baseline from disagreeing with the files: a divergence would make
/// [`WallpaperModel::reset`] restore a toggle state the config does not have, turning a
/// clean page into a pending change the user never made (R5.1).
fn override_from_paths(wallpaper: Option<&str>, lock: Option<&str>, home: Option<&Path>) -> bool {
    match (wallpaper, lock) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(wallpaper), Some(lock)) => expand_home(wallpaper, home) != expand_home(lock, home),
    }
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
        let backing = BackingSet::load(&[
            paths.hyprpaper_conf.as_path(),
            paths.hyprlock_conf.as_path(),
        ]);

        let hyprpaper_file = backing
            .get(&paths.hyprpaper_conf)
            .map(|backing| HyprlangFile::parse(&backing.text).0);
        let hyprlock_file = backing
            .get(&paths.hyprlock_conf)
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

        let override_initial = override_from_paths(
            wallpaper_path.as_deref(),
            lock_path.as_deref(),
            paths.home.as_deref(),
        );

        let fit_options: Vec<String> = CURATED_FIT_MODES.iter().map(|s| (*s).to_string()).collect();

        tracing::info!(
            wallpaper = wallpaper_path.is_some(),
            lock = lock_path.is_some(),
            override_on = override_initial,
            lock_available,
            "loaded wallpaper / lock background for the Theme page (task 6.5)"
        );

        WallpaperModel {
            backing,
            wallpaper: Staged::new(wallpaper_path),
            fit: Selection::new(fit_options, fit),
            lock: Staged::new(lock_path),
            override_on: override_initial,
            override_initial,
            lock_available,
            paths,
        }
    }

    /// `hyprpaper.conf`'s current text, or `None` when it was unreadable (R4.4) — the
    /// wallpaper rows are then hidden.
    fn hyprpaper(&self) -> Option<&BackingText> {
        self.backing.get(&self.paths.hyprpaper_conf)
    }

    /// `hyprlock.conf`'s current text, or `None` when it was unreadable (R4.4) — the lock
    /// override is then unavailable.
    fn hyprlock(&self) -> Option<&BackingText> {
        self.backing.get(&self.paths.hyprlock_conf)
    }

    /// Whether the wallpaper rows should be shown: `hyprpaper.conf` was readable, so a
    /// path/fit edit can be written (R4.4).
    pub fn wallpaper_editable(&self) -> bool {
        self.hyprpaper().is_some()
    }

    /// Whether the lock-screen override control should be shown: hyprlock is present and
    /// `hyprlock.conf` is readable, so an override can be written (R4.2/R4.4).
    pub fn lock_editable(&self) -> bool {
        self.lock_available && self.hyprlock().is_some()
    }

    /// The effective wallpaper image path (staged or current) as written in
    /// `hyprpaper.conf` — possibly home-relative (`~/…`) — or `None` when unset. For
    /// display; use [`Self::wallpaper_file`] to reach the file itself.
    pub fn wallpaper_path(&self) -> Option<&str> {
        self.wallpaper.effective()
    }

    /// The effective wallpaper image as a filesystem path, with a home-relative value
    /// expanded (task 11.10) — what the UI's file chooser starts from.
    pub fn wallpaper_file(&self) -> Option<PathBuf> {
        self.wallpaper.effective().map(|path| self.expand(path))
    }

    /// The fit-mode drop-down options (the curated modes plus the current value).
    pub fn fit_options(&self) -> &[String] {
        &self.fit.options
    }

    /// The preselected index of the fit-mode drop-down.
    pub fn selected_fit_index(&self) -> Option<usize> {
        self.fit.selected_index()
    }

    /// The note the Theme page shows under the fit-mode drop-down while the effective
    /// fit (the staged one, else the configured one) is `fill`, or `None` otherwise
    /// (task 11.14).
    ///
    /// `hyprpaper.conf` accepts `fill` and hyprpaper renders it as a stretch at
    /// start-up, but the live wallpaper request the Apply sends cannot express it:
    /// over hyprpaper's IPC only `contain` and `tile` render differently from the
    /// default `cover`, so the reload leaves the fit out for every other mode (see
    /// [`ReloadAction::HyprpaperWallpaper`](crate::core::reload::ReloadAction::HyprpaperWallpaper)).
    /// The file keeps `fill`; only the live preview differs until hyprpaper next
    /// starts, which the note says. A configured `fill` that is being changed to
    /// another mode gets no note, because that mode does show live.
    pub fn fit_preview_note(&self) -> Option<String> {
        (self.fit.effective() == Some(FILL_FIT_MODE)).then(|| {
            "The live preview shows \"fill\" as \"cover\" until hyprpaper next starts: \
             changing the wallpaper while it runs only renders \"contain\" and \"tile\" \
             differently."
                .to_string()
        })
    }

    /// Whether the lock-screen override is on (a different image than the wallpaper).
    pub fn override_on(&self) -> bool {
        self.override_on
    }

    /// The effective lock-screen override image path (staged or current) as written in
    /// `hyprlock.conf`, or `None`. Only meaningful while [`Self::override_on`] is set;
    /// the UI shows it in the override chooser.
    pub fn lock_path(&self) -> Option<&str> {
        self.lock.effective()
    }

    /// The effective lock-screen override image as a filesystem path, with a
    /// home-relative value expanded (task 11.10), like [`Self::wallpaper_file`].
    pub fn lock_file(&self) -> Option<PathBuf> {
        self.lock.effective().map(|path| self.expand(path))
    }

    /// Expands a home-relative config value against the injected home directory.
    fn expand(&self, path: &str) -> PathBuf {
        expand_home(path, self.paths.home.as_deref())
    }

    /// Stages a wallpaper image path after validating it (R8.3).
    ///
    /// The path must exist, be readable, and have an image extension, and every place it
    /// is written must be able to hold it — `hyprpaper.conf` and `hyprlock.conf` refuse
    /// a line break or a `#` (hyprlang reads `#` as the start of a comment), and the
    /// live `hyprctl hyprpaper` request refuses a `,` (task 9.27; see
    /// [`validate_wallpaper_value`]). An invalid path is rejected (returned as an
    /// [`Err`] whose message names the problem, which the UI surfaces) and nothing is
    /// staged, so a broken wallpaper can never be written — nor a path only some of its
    /// copies would take. `path` is usually the absolute path the file chooser
    /// returned; it is staged in `hyprpaper.conf`'s own form (see "Home-relative paths"
    /// on [`WallpaperModel`]). The pipeline re-runs the same check in step 1 through
    /// the contribution's [`validations`](WallpaperApply::validations).
    ///
    /// Unlike the theme drop-downs, re-picking the path already on disk is checked like
    /// any other choice — the image must still exist, so there is no exemption for the
    /// current value — and a configured path holding a refused character (a `#` or `,`
    /// written by hand) is therefore refused too. That leaves nothing stuck: the refusal
    /// stages nothing, and Reset still returns the page to the on-disk path.
    pub fn stage_wallpaper(&mut self, path: &str) -> Result<(), ValidationError> {
        let home = self.paths.home.as_deref();
        validate_wallpaper_value(path, home)?;
        let in_form = path_in_form_of(path, self.wallpaper.original.as_deref(), home);
        self.wallpaper.stage(&in_form);
        Ok(())
    }

    /// Stages a lock-screen override image path after validating it (R8.3), like
    /// [`Self::stage_wallpaper`] but against `hyprlock.conf` alone, the only file it is
    /// written to ([`validate_lock_background_value`]), in `hyprlock.conf`'s own form.
    pub fn stage_lock(&mut self, path: &str) -> Result<(), ValidationError> {
        let home = self.paths.home.as_deref();
        validate_lock_background_value(path, home)?;
        let in_form = path_in_form_of(path, self.lock.original.as_deref(), home);
        self.lock.stage(&in_form);
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
        self.wallpaper_editable() && (self.wallpaper.is_dirty() || self.fit.is_dirty())
    }

    /// The image path that should be written to `hyprlock.conf`: the override path when
    /// the override is on, otherwise the wallpaper path (so the two stay in sync).
    ///
    /// The wallpaper path is in `hyprpaper.conf`'s form, so following it re-expresses it
    /// in `hyprlock.conf`'s own form (task 11.10): the two files may legitimately differ
    /// in form while naming the same image, and that must neither trigger a write nor
    /// change the lock file's form.
    fn effective_lock(&self) -> Option<String> {
        if self.override_on {
            self.lock.effective().map(str::to_string)
        } else {
            self.wallpaper.effective().map(|path| {
                path_in_form_of(
                    path,
                    self.lock.original.as_deref(),
                    self.paths.home.as_deref(),
                )
            })
        }
    }

    /// Whether `hyprlock.conf` needs writing — the effective lock path differs from
    /// what is on disk, hyprlock is editable, and there is a path to write.
    fn hyprlock_write_needed(&self) -> bool {
        if !self.lock_editable() {
            return false;
        }
        match self.effective_lock() {
            Some(path) => Some(path.as_str()) != self.lock.original.as_deref(),
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

    /// Whether either backing file changed on disk since it was loaded, so a dirty change
    /// must be reloaded instead of written (R5.6) — see [`BackingSet::check_conflict`] for
    /// the discipline this is part of.
    pub fn check_conflict(&self) -> bool {
        self.backing.check_conflict()
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

    /// Commits the staged changes after a successful Apply: takes the written bytes back
    /// into the backing set (re-baselining freshness and the in-memory text, see
    /// [`BackingSet::absorb_writes`]) and promotes the staged values **that actually
    /// reached a file** to their current value (R5.6).
    ///
    /// Promotion is derived from the rendered writes, not from the
    /// `*_write_needed` predicates: a value whose hyprlang edit was skipped (see
    /// [`WrittenWallpaperValues`]) stays staged, so the page stays dirty and the user can
    /// retry. Promoting it would leave the model claiming a path that reached no file, and
    /// because the value would then no longer look changed, no later Apply would write it
    /// — a permanent silent divergence between the UI and the config.
    pub fn commit(&mut self) {
        let (writes, written) = self.render_writes();
        self.backing.absorb_writes(&writes);
        // Capture the effective lock path (which depends on the still-staged wallpaper
        // value when the override is off) before promoting the wallpaper edit.
        let new_lock = self.effective_lock();
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
            self.paths.home.as_deref(),
        );
    }

    /// Renders the file writes for the current staged changes, together with which values
    /// actually reached one of them (used by both [`Self::apply_contribution`], which
    /// needs only the writes, and [`Self::commit`], which needs the promotion record).
    fn render_writes(&self) -> (Vec<FileWrite>, WrittenWallpaperValues) {
        let mut writes = Vec::new();
        let mut written = WrittenWallpaperValues::default();
        if self.hyprpaper_write_needed() {
            if let Some(backing) = self.hyprpaper() {
                let (write, keys) = self.render_hyprpaper(backing);
                written.merge(keys);
                writes.extend(write);
            }
        }
        if self.hyprlock_write_needed() {
            if let Some(backing) = self.hyprlock() {
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

        if self.wallpaper.is_dirty() {
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
        if self.fit.is_dirty() {
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
                // Tasks 9.10, 9.27: this write carries a validated value only when it
                // carries the wallpaper *path* — [`Self::validations`] re-checks that
                // path (it must still exist, be readable and be holdable by every
                // writer, R8.3) under the same condition that put it in this write. A
                // fit-only change carries just a mode from a fixed list, and
                // deliberately does not drag the unchanged path into validation, so
                // such a write declares no setting.
                validated_by: if written.wallpaper {
                    vec![SettingId::WallpaperPath]
                } else {
                    Vec::new()
                },
                mode: WriteMode::Replace,
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
        match file.set_value(&KeyPath::at(&[LOCK_SECTION], LOCK_PATH_KEY), &path) {
            Ok(()) => Some(FileWrite {
                path: backing.path.clone(),
                contents: file.emit().into_bytes(),
                changed_keys: vec!["lock background path".to_string()],
                backing: BackingFile::HyprlockConf,
                // This write exists only when there is an effective lock path, which is
                // exactly the condition under which [`Self::validations`] re-checks that
                // path (R8.3) — so its value is always covered by the plan.
                validated_by: vec![SettingId::LockBackgroundPath],
                mode: WriteMode::Replace,
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
    /// written, so the live `hyprctl hyprpaper wallpaper ,<path>[,<fit>]` applies the
    /// current fit (a fit-only change re-sets the same image with the new fit; which
    /// fit values reach the IPC is decided by the reload action). A
    /// hyprlock-only change carries no wallpaper parameter — and, combined with
    /// [`BackingFile::HyprlockConf`] mapping to no action, issues no reload command at
    /// all (the intentional hyprlock behaviour, architecture §6).
    ///
    /// The path is expanded to its absolute form even when the file stores `~/…`,
    /// because hyprpaper's IPC accepts only absolute paths (see "Home-relative paths" on
    /// [`WallpaperModel`]). Only with an unknown home does a `~/…` value pass through
    /// unexpanded; it can then never have been validated, so it only reaches the
    /// reload on a fit-only change, where `hyprctl` expands it from its own `$HOME`.
    fn reload_params(&self) -> ReloadParams {
        let home = self.paths.home.as_deref();
        let (wallpaper, fit) = if self.hyprpaper_write_needed() {
            (
                self.wallpaper
                    .effective()
                    .map(|path| expand_home_to_string(path, home)),
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
    ///
    /// Each path is handed over expanded against the injected home (task 11.10), so the
    /// Apply-time re-check examines the same file staging did; a `~/…` value is left as
    /// is only when the home is unknown, and [`SettingId::validate`] then rejects it.
    fn validations(&self) -> Vec<(SettingId, Value)> {
        let home = self.paths.home.as_deref();
        let mut validations = Vec::new();
        if self.wallpaper.is_dirty() {
            if let Some(path) = self.wallpaper.effective() {
                validations.push((
                    SettingId::WallpaperPath,
                    Value::String(expand_home_to_string(path, home)),
                ));
            }
        }
        if self.hyprlock_write_needed() {
            if let Some(path) = self.effective_lock() {
                validations.push((
                    SettingId::LockBackgroundPath,
                    Value::String(expand_home_to_string(&path, home)),
                ));
            }
        }
        validations
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
    /// [`WallpaperPaths`], with `dir/home` as the home directory the configs' `~/…`
    /// paths expand against.
    ///
    /// The tests write their images with [`write_image`] *beside* `dir`, i.e. outside
    /// that home, so a chosen image is written absolute even into these tilde-shaped
    /// configs; the home-relative write is covered by the tests that place an image
    /// under [`home_of`] explicitly.
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
            home: Some(dir.join("home")),
        }
    }

    /// The home directory of a [`WallpaperPaths`] built by these helpers.
    fn home_of(paths: &WallpaperPaths) -> &Path {
        paths
            .home
            .as_deref()
            .expect("the test fixtures always set a home")
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
    fn a_wallpaper_plus_fit_change_writes_both_files() {
        // Accept criterion: a staged wallpaper path + fit-mode edit produces the exact
        // surgical hyprpaper.conf/hyprlock.conf FileWrite diffs AND the single hyprctl
        // hyprpaper wallpaper request (task 11.14). With no override, the same path is written
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
                .all(|write| !write.validated_by.is_empty()),
            "a path-carrying write declares the path setting the plan validates"
        );

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: Vec::new(),
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

        // The reload: one request setting the image on all outputs (empty monitor
        // field before the comma) with the staged fit as the third comma-field, so the
        // fit is applied live (task 6.5) — no `preload` step (task 11.14). No hyprlock
        // reload (see the dedicated test).
        let set_arg = format!(",{wall},contain");
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").args(["hyprpaper", "wallpaper", set_arg.as_str()])]
        );
    }

    #[test]
    fn the_fit_preview_note_appears_only_while_fill_is_in_effect() {
        // Task 11.14: the live wallpaper request cannot render `fill`, so the page says
        // so while `fill` is staged or configured — and not for the modes that do show
        // live, nor once a configured `fill` is being changed away from.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);

        let mut model = WallpaperModel::load(paths.clone(), true);
        assert_eq!(
            model.fit_preview_note(),
            None,
            "the fixture's cover shows live"
        );
        for fit in ["contain", "tile", "cover"] {
            model.stage_fit(fit);
            assert_eq!(model.fit_preview_note(), None, "{fit} shows live");
        }
        model.stage_fit("fill");
        let note = model.fit_preview_note().expect("a staged fill is noted");
        assert!(note.contains("until hyprpaper next starts"), "got {note}");
        model.reset();
        assert_eq!(
            model.fit_preview_note(),
            None,
            "reset drops the staged fill"
        );

        // A file that already says `fill` is noted with nothing staged.
        fs::write(
            &paths.hyprpaper_conf,
            HYPRPAPER_CONF.replace("fit_mode = cover", "fit_mode = fill"),
        )
        .expect("configure fill");
        let mut model = WallpaperModel::load(paths, true);
        assert!(
            model.fit_preview_note().is_some(),
            "a configured fill is noted"
        );
        model.stage_fit("tile");
        assert_eq!(model.fit_preview_note(), None, "changing away shows live");
    }

    #[test]
    fn a_fit_only_change_reloads_hyprpaper_with_the_new_fit() {
        // MAJOR-fix (task 6.5): a fit-only change still reloads hyprpaper — it re-sets
        // the (unchanged) wallpaper image with the NEW fit as the third comma-field, so
        // the fit takes effect live rather than only on the next hyprpaper restart.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);

        // The reload must name the configured `~/…` image by its absolute path (task
        // 11.10): hyprpaper's IPC rejects anything that does not start with `/`.
        let absolute = home_of(&paths)
            .join("Pictures/wallpaper/18.jpg")
            .to_string_lossy()
            .into_owned();
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
        // write must declare no setting — otherwise the Apply pipeline's plan-drift
        // guard would warn "the caller dropped its validations" on every fit-only
        // change.
        assert!(
            contribution.writes[0].validated_by.is_empty(),
            "a fit-only write carries nothing to validate and must say so"
        );
        assert_eq!(
            contribution.reload_params.wallpaper.as_deref(),
            Some(absolute.as_str())
        );
        assert_eq!(contribution.reload_params.fit.as_deref(), Some("tile"));

        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: Vec::new(),
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
        let set_arg = format!(",{absolute},tile");
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").args(["hyprpaper", "wallpaper", set_arg.as_str()])]
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
            links: Vec::new(),
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
    fn staging_refuses_a_hash_naming_both_hyprlang_files() {
        // Task 9.27 accept criterion: a `#`-bearing image is a real, readable file, but
        // hyprpaper.conf and hyprlock.conf would truncate the path at the `#`, so the
        // choice is refused with a message naming the character and both writers, and
        // nothing is staged.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_wallpaper_fixture(&tmp.path().join("config"));
        let hashed = write_image(tmp.path(), "wall#1.png");
        let mut model = WallpaperModel::load(paths, true);

        let message = model
            .stage_wallpaper(&hashed)
            .expect_err("a `#` in the path is refused")
            .to_string();
        assert!(
            message.contains("`#`")
                && message.contains("hyprpaper.conf")
                && message.contains("hyprlock.conf"),
            "the message names the character and the writers: {message}"
        );
        let lock_message = model
            .stage_lock(&hashed)
            .expect_err("the separate lock image follows hyprlock.conf's rule")
            .to_string();
        assert!(
            lock_message.contains("`#`") && lock_message.contains("hyprlock.conf"),
            "{lock_message}"
        );
        assert!(!model.is_dirty(), "a refused path is never staged");
    }

    #[test]
    fn a_comma_is_refused_for_the_wallpaper_but_not_the_lock_image() {
        // The live `hyprctl hyprpaper wallpaper ,<path>` request splits on commas, so the
        // wallpaper cannot hold one; a separate lock image never reaches that request
        // and hyprlang itself accepts a comma.
        let tmp = tempfile::tempdir().expect("temp dir");
        let paths = write_wallpaper_fixture(&tmp.path().join("config"));
        let comma = write_image(tmp.path(), "wall,1.png");
        let mut model = WallpaperModel::load(paths, true);

        let message = model
            .stage_wallpaper(&comma)
            .expect_err("a comma in the wallpaper path is refused")
            .to_string();
        assert!(
            message.contains("`,`") && message.contains("hyprctl hyprpaper"),
            "{message}"
        );
        model.set_override(true);
        model
            .stage_lock(&comma)
            .expect("a separate lock image may hold a comma");
        assert!(model.is_dirty());
    }

    #[test]
    fn re_choosing_the_configured_wallpaper_leaves_the_page_clean() {
        // The dirty rule for the two free-form image paths, which share their staging core
        // with the drop-downs (`Staged`): re-choosing the path the config already holds
        // clears the pending edit, so a user who opens the file chooser and picks the
        // current wallpaper again does not light up Apply (R5.1). The fixture's configured
        // path has to be a real image here, because staging re-validates it (R8.3).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let current = write_image(tmp.path(), "current.png");
        let other = write_image(tmp.path(), "other.png");
        fs::write(
            &paths.hyprpaper_conf,
            HYPRPAPER_CONF.replace("~/Pictures/wallpaper/18.jpg", &current),
        )
        .expect("point the fixture's wallpaper at a real image");

        let mut model = WallpaperModel::load(paths, true);
        assert_eq!(model.wallpaper_path(), Some(current.as_str()));
        assert!(!model.is_dirty(), "a freshly loaded page is clean");

        model
            .stage_wallpaper(&other)
            .expect("a real image validates");
        assert!(model.is_dirty(), "a different image is a pending change");

        model
            .stage_wallpaper(&current)
            .expect("the configured image validates too");
        assert!(
            !model.is_dirty(),
            "re-choosing the configured wallpaper cancels the pending change"
        );
        assert_eq!(
            model.wallpaper_path(),
            Some(current.as_str()),
            "and the effective path is the configured one again"
        );
        assert!(
            model.apply_contribution().is_none(),
            "a clean page contributes nothing to an Apply"
        );
    }

    #[test]
    fn the_lock_override_hides_when_hyprlock_is_absent() {
        // R4.2: the lock-override control is hidden when hyprlock is absent.
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
    }

    #[test]
    fn the_wallpaper_rows_hide_when_hyprpaper_conf_is_unreadable() {
        // R4.4: the wallpaper rows are hidden when hyprpaper.conf is unreadable.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);

        let unreadable = WallpaperModel::load(
            WallpaperPaths {
                hyprpaper_conf: config.join("hypr").join("does-not-exist.conf"),
                hyprlock_conf: paths.hyprlock_conf.clone(),
                home: paths.home.clone(),
            },
            true,
        );
        assert!(
            !unreadable.wallpaper_editable(),
            "an unreadable hyprpaper.conf hides the wallpaper rows (R4.4)"
        );
    }

    #[test]
    fn reset_restores_the_override_toggle() {
        // Reset restores the toggle baseline and clears staged paths (R5.1).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
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
    }

    #[test]
    fn a_committed_wallpaper_write_is_not_a_self_conflict() {
        // After a committed apply the app's own write is not seen as an external
        // conflict (R5.6).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let wall = write_image(tmp.path(), "wall.png");
        let mut model = WallpaperModel::load(paths, true);

        // Stage a wallpaper change, apply it, and commit.
        model.stage_wallpaper(&wall).unwrap();
        let contribution = model.apply_contribution().expect("a change contributes");
        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: Vec::new(),
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
                home: Some(config.join("home")),
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
    fn a_lock_override_writes_each_file_its_own_path() {
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
            links: Vec::new(),
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
    fn turning_the_override_off_resyncs_the_lock_image() {
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
            links: Vec::new(),
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
    fn a_rejected_path_stays_dirty_while_the_fit_is_promoted() {
        // Task 9.6, per-key promotion *within* one file: the hyprlang writer refuses an
        // image path containing a `#`, because hyprlang would read it as the start of an
        // inline comment and silently truncate the path. Since task 9.27
        // `stage_wallpaper` refuses such a path up front, so this test stages it into the
        // field directly to keep the writer's defence-in-depth path covered. The fit mode
        // staged in the same file is written regardless, so commit promotes the fit and
        // leaves the wallpaper path staged.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let hashed = write_image(tmp.path(), "wall#1.png");

        let mut model = WallpaperModel::load(paths, true);
        model.wallpaper.stage(&hashed);
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
            model.wallpaper.is_dirty() && model.is_dirty(),
            "the rejected wallpaper path stays staged and the page stays dirty"
        );
    }

    #[test]
    fn a_hyprpaper_only_write_rebaselines_the_override() {
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
            links: Vec::new(),
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

    /// Creates `relative` (with parents) below `home` as a stub image file and returns
    /// its absolute path string — the shape a file chooser hands back.
    fn write_home_image(home: &Path, relative: &str) -> String {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().expect("an image path has a parent"))
            .expect("create the image's directory");
        fs::write(&path, b"img").expect("write image file");
        path.to_string_lossy().into_owned()
    }

    /// Runs a contribution through the real Apply pipeline with a mock runner and
    /// returns the commands it recorded.
    fn apply_and_record(contribution: WallpaperApply) -> Vec<Command> {
        let plan = ApplyPlan {
            validations: contribution.validations,
            writes: contribution.writes,
            links: Vec::new(),
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
            "the wallpaper apply must succeed, got {outcome:?}"
        );
        runner.recorded()
    }

    #[test]
    fn an_image_under_home_is_written_tilde_relative() {
        // Task 11.10: both fixture files store `~/…`, so a chosen image below the home
        // directory is written as `~/…` into each, while validation and the hyprpaper
        // reload work on the absolute path.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let chosen = write_home_image(home_of(&paths), "Pictures/next.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        model
            .stage_wallpaper(&chosen)
            .expect("an image under home validates");
        assert_eq!(model.wallpaper_path(), Some("~/Pictures/next.png"));
        assert_eq!(model.wallpaper_file(), Some(PathBuf::from(&chosen)));

        let contribution = model.apply_contribution().expect("a change contributes");
        assert_eq!(
            contribution.validations,
            vec![
                (SettingId::WallpaperPath, Value::String(chosen.clone())),
                (SettingId::LockBackgroundPath, Value::String(chosen.clone())),
            ],
            "the Apply-time re-check examines the expanded paths"
        );
        let recorded = apply_and_record(contribution);

        assert_eq!(
            fs::read_to_string(&paths.hyprpaper_conf).unwrap(),
            HYPRPAPER_CONF.replace("~/Pictures/wallpaper/18.jpg", "~/Pictures/next.png")
        );
        assert_eq!(
            fs::read_to_string(&paths.hyprlock_conf).unwrap(),
            HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", "~/Pictures/next.png")
        );
        // `cover` is hyprpaper's IPC default, so the fit field is left out (task 11.14).
        let set_arg = format!(",{chosen}");
        assert_eq!(
            recorded,
            vec![Command::new("hyprctl").args(["hyprpaper", "wallpaper", set_arg.as_str()])],
            "hyprpaper's IPC receives the absolute path, never `~/…`"
        );
    }

    #[test]
    fn an_absolute_config_keeps_writing_absolute_paths() {
        // Task 11.10: a file whose current value is absolute keeps the absolute form even
        // for an image below the home directory.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let current = write_home_image(home_of(&paths), "Pictures/wallpaper/18.jpg");
        fs::write(
            &paths.hyprpaper_conf,
            HYPRPAPER_CONF.replace("~/Pictures/wallpaper/18.jpg", &current),
        )
        .expect("write an absolute hyprpaper.conf");
        fs::write(
            &paths.hyprlock_conf,
            HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &current),
        )
        .expect("write an absolute hyprlock.conf");
        let chosen = write_home_image(home_of(&paths), "Pictures/next.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        model
            .stage_wallpaper(&chosen)
            .expect("an image under home validates");
        apply_and_record(model.apply_contribution().expect("a change contributes"));

        assert_eq!(
            fs::read_to_string(&paths.hyprpaper_conf).unwrap(),
            HYPRPAPER_CONF.replace("~/Pictures/wallpaper/18.jpg", &chosen)
        );
        assert_eq!(
            fs::read_to_string(&paths.hyprlock_conf).unwrap(),
            HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &chosen)
        );
    }

    #[test]
    fn each_file_keeps_its_own_form_for_the_same_image() {
        // Task 11.10: the two files decide independently. `~/…` in hyprpaper.conf and the
        // absolute spelling of the same image in hyprlock.conf are one image (no
        // override, nothing dirty), and a new image is written in each file's own form.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let current = write_home_image(home_of(&paths), "Pictures/wallpaper/18.jpg");
        fs::write(
            &paths.hyprlock_conf,
            HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &current),
        )
        .expect("write an absolute hyprlock.conf");
        let chosen = write_home_image(home_of(&paths), "Pictures/next.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        assert!(
            !model.override_on(),
            "two spellings of one image are the unified default"
        );
        assert!(
            !model.is_dirty(),
            "a differing form alone is no pending change"
        );

        model
            .stage_wallpaper(&chosen)
            .expect("an image under home validates");
        apply_and_record(model.apply_contribution().expect("a change contributes"));

        assert_eq!(
            fs::read_to_string(&paths.hyprpaper_conf).unwrap(),
            HYPRPAPER_CONF.replace("~/Pictures/wallpaper/18.jpg", "~/Pictures/next.png")
        );
        assert_eq!(
            fs::read_to_string(&paths.hyprlock_conf).unwrap(),
            HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", &chosen)
        );
        model.commit();
        assert!(!model.is_dirty(), "the committed page is clean");
        assert!(
            !model.override_on(),
            "the two files still name one image after the write"
        );
    }

    #[test]
    fn each_file_keeps_its_own_form_in_the_reverse_pairing() {
        // The mirror of the test above: an absolute wallpaper next to a `~/…` lock
        // background. Following the absolute wallpaper must contract it back to `~/…`
        // for hyprlock.conf (the contraction branch of the lock path), and the two
        // spellings must still read as one image.
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let current = write_home_image(home_of(&paths), "Pictures/wallpaper/18.jpg");
        fs::write(
            &paths.hyprpaper_conf,
            HYPRPAPER_CONF.replace("~/Pictures/wallpaper/18.jpg", &current),
        )
        .expect("write an absolute hyprpaper.conf");
        let chosen = write_home_image(home_of(&paths), "Pictures/next.png");

        let mut model = WallpaperModel::load(paths.clone(), true);
        assert!(
            !model.override_on(),
            "two spellings of one image are the unified default"
        );
        assert!(
            !model.is_dirty(),
            "a differing form alone is no pending change"
        );

        model
            .stage_wallpaper(&chosen)
            .expect("an image under home validates");
        assert_eq!(model.wallpaper_path(), Some(chosen.as_str()));
        apply_and_record(model.apply_contribution().expect("a change contributes"));

        assert_eq!(
            fs::read_to_string(&paths.hyprpaper_conf).unwrap(),
            HYPRPAPER_CONF.replace("~/Pictures/wallpaper/18.jpg", &chosen)
        );
        assert_eq!(
            fs::read_to_string(&paths.hyprlock_conf).unwrap(),
            HYPRLOCK_CONF.replace("~/Pictures/wallpaper/18.jpg", "~/Pictures/next.png")
        );
        model.commit();
        assert!(!model.is_dirty(), "the committed page is clean");
        assert!(
            !model.override_on(),
            "the two files still name one image after the write"
        );
    }

    #[test]
    fn re_choosing_the_current_image_by_absolute_path_is_clean() {
        // The chooser returns absolute paths; picking the image the config names as
        // `~/…` again must not light up Apply (R5.1).
        let tmp = tempfile::tempdir().expect("temp dir");
        let config = tmp.path().join("config");
        let paths = write_wallpaper_fixture(&config);
        let current = write_home_image(home_of(&paths), "Pictures/wallpaper/18.jpg");

        let mut model = WallpaperModel::load(paths, true);
        assert_eq!(model.wallpaper_file(), Some(PathBuf::from(&current)));
        model
            .stage_wallpaper(&current)
            .expect("the configured image validates");
        assert!(!model.is_dirty());
        assert_eq!(model.wallpaper_path(), Some("~/Pictures/wallpaper/18.jpg"));
    }

    /// Loads a [`WallpaperModel`] over the standard fixture with the home directory
    /// unknown, for the two tests below.
    fn homeless_wallpaper_model(tmp: &Path) -> WallpaperModel {
        let config = tmp.join("config");
        let paths = WallpaperPaths {
            home: None,
            ..write_wallpaper_fixture(&config)
        };
        WallpaperModel::load(paths, true)
    }

    #[test]
    fn without_a_home_a_tilde_choice_is_rejected() {
        // An unknown home directory: a home-relative value names no checkable file, so
        // it is rejected (R8.3).
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = homeless_wallpaper_model(tmp.path());
        assert!(matches!(
            model.stage_wallpaper("~/Pictures/wallpaper/18.jpg"),
            Err(ValidationError::ImagePath {
                problem: crate::core::model::ImagePathProblem::HomeUnknown,
                ..
            })
        ));
    }

    #[test]
    fn without_a_home_an_absolute_choice_still_works() {
        // An unknown home directory does not matter to an absolute image, which is
        // staged and written as is.
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut model = homeless_wallpaper_model(tmp.path());
        let wall = write_image(tmp.path(), "wall.png");
        model
            .stage_wallpaper(&wall)
            .expect("an absolute image needs no home");
        assert_eq!(model.wallpaper_path(), Some(wall.as_str()));
    }
}
