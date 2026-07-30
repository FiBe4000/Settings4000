//! The Theme page's GTK-free domain models: palette-scheme switching (task 6.3),
//! GTK/icon/cursor theme selection (task 6.4), and wallpaper / lock-screen background
//! (task 6.5) — architecture §5, §6, §7; R2.2, R3.2, R3.3, R3.4, R4.2, R4.4, R8.3,
//! R8.5, R6.2.
//!
//! # The three models here
//!
//! The Theme page is built from independent sections, each backed by its own GTK-free
//! staging model in its own submodule of this one:
//!
//! - [`PaletteModel`] (submodule `palette`, task 6.3) — switching the central color
//!   palette, which runs `scripts/generate-colors` rather than editing a file;
//! - [`ThemesModel`] (submodule `themes`, task 6.4) — the GTK theme, icon theme, cursor
//!   theme, and cursor size drop-downs, each written identically to every file that
//!   duplicates the value (R3.3/R3.4);
//! - [`WallpaperModel`] (submodule `wallpaper`, task 6.5) — the desktop wallpaper
//!   (`hyprpaper.conf`) and the lock-screen background (`hyprlock.conf`).
//!
//! The models are independent: they share only the small staging primitives that stay in
//! this file (`Staged` and its drop-down wrapper `Selection` for values, `BackingSet` and
//! `BackingText` for the config files behind them — private, hence named rather than
//! linked). Each submodule's own module docs, and the docs on each model type, carry the
//! detail — the list above exists only so a reader lands in the right submodule. The
//! models are re-exported here, so every caller keeps addressing them as
//! `core::theme::<Model>` regardless of which submodule they live in.
//!
//! # Why bespoke models, not `SettingId`s in the store
//!
//! All three are bespoke staging sources (like the Display page's per-monitor model,
//! [`crate::core::display`]) that the window folds into the shared Apply/Reset chrome and
//! the same [`apply::run`](crate::core::apply::run) pipeline, rather than store-backed
//! [`SettingId`](crate::core::model::SettingId) values. The store's shape — an
//! `original`/`staged` [`Value`](crate::core::model::Value) per `SettingId`, applied as
//! one [`FileWrite`](crate::core::apply::FileWrite) per file — does not fit a control
//! whose value is duplicated across four files, one whose Apply runs a generator instead
//! of writing a file (see the `palette` submodule for that argument in full), or one
//! whose two files hold a derived rather than an edited value.
//!
//! Everything here lives in `core/` so enumeration, staging, the multi-file writes, and
//! the reload decisions are headlessly testable (R6.2) — the layering guard in
//! `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import. Every path the models
//! read or write is injected, so tests drive them against temporary fixture trees with no
//! live dotfiles deployment.

use std::path::{Path, PathBuf};

use crate::core::apply::FileWrite;
use crate::core::freshness::FreshnessTracker;

mod palette;
mod themes;
mod wallpaper;

pub use palette::{PaletteModel, Scheme};
pub use themes::{GtkThemeOverrideSource, ThemeRoots, ThemesApply, ThemesModel, ThemesPaths};
pub use wallpaper::{WallpaperApply, WallpaperModel, WallpaperPaths};

/// One staged value: what the backing config currently holds, plus any pending edit.
///
/// This is the staging core every Theme-page control is built from — the four theme
/// drop-downs through the [`Selection`] wrapper that adds their candidate options, the
/// wallpaper and lock-screen image paths directly (they are free-form values the user
/// picks with a file chooser, so there is no option list). It mirrors the store's
/// `original`/`staged` dirty rule: re-staging the current value clears the pending edit,
/// so it never lights up Apply.
///
/// Values are held as strings even when they are conceptually numeric (the cursor size),
/// so one dirty rule serves every control; such a value is parsed to a number only when
/// it is written or handed to a reload command.
#[derive(Clone, Debug)]
struct Staged {
    /// The value read from the backing config, or `None` when the config did not set
    /// it. Staging any value while this is `None` counts as a change (a write that
    /// appends the key).
    original: Option<String>,
    /// The pending value, or `None` when nothing is staged. Only ever set to a value
    /// that differs from [`original`](Self::original), so `staged.is_some()` is exactly
    /// the dirty condition.
    staged: Option<String>,
}

impl Staged {
    /// Builds a clean staged value over what the config currently holds.
    fn new(original: Option<String>) -> Self {
        Staged {
            original,
            staged: None,
        }
    }

    /// The effective value — the pending edit if there is one, else the current value.
    fn effective(&self) -> Option<&str> {
        self.staged.as_deref().or(self.original.as_deref())
    }

    /// Stages `value`, clearing the pending edit when it equals the current value (so
    /// re-staging the current value is not dirty).
    fn stage(&mut self, value: &str) {
        if self.original.as_deref() == Some(value) {
            self.staged = None;
        } else {
            self.staged = Some(value.to_string());
        }
    }

    /// Whether an edit differing from the current value is pending.
    fn is_changed(&self) -> bool {
        self.staged.is_some()
    }

    /// Discards the pending edit.
    fn reset(&mut self) {
        self.staged = None;
    }

    /// Promotes the pending edit to the current value after a committed Apply.
    fn commit(&mut self) {
        if let Some(value) = self.staged.take() {
            self.original = Some(value);
        }
    }
}

/// One drop-down's staged selection: a [`Staged`] value plus the options offered for it.
///
/// Used for all four theme controls (GTK theme, icon theme, cursor theme, cursor size)
/// and the wallpaper fit mode. The staging behaviour is entirely [`Staged`]'s — the
/// methods below forward to it — so the only thing this type adds is the option list and
/// the index lookup the UI needs to preselect the drop-down.
#[derive(Clone, Debug)]
struct Selection {
    /// The drop-down's candidate values, in display order. Always includes the current
    /// value (prepended when discovery did not surface it) so it stays selectable.
    options: Vec<String>,
    /// The selected value's staging state.
    value: Staged,
}

impl Selection {
    /// Builds a selection over `options`, ensuring `original` is selectable.
    ///
    /// If the current value is not among the discovered options (e.g. a theme
    /// installed somewhere unusual, or a config value with no matching installed
    /// theme), it is prepended so the drop-down can still preselect it — the same
    /// "keep the configured value selectable" rule the Display page's scale/position
    /// drop-downs follow.
    fn new(mut options: Vec<String>, original: Option<String>) -> Self {
        if let Some(current) = &original {
            if !options.iter().any(|option| option == current) {
                options.insert(0, current.clone());
            }
        }
        Selection {
            options,
            value: Staged::new(original),
        }
    }

    /// The effective value — see [`Staged::effective`].
    fn effective(&self) -> Option<&str> {
        self.value.effective()
    }

    /// The index of the effective value within [`options`](Self::options), for
    /// preselecting the drop-down. `None` when the effective value is not among the
    /// options (which cannot happen once [`new`](Self::new) has made `original`
    /// selectable, but is handled without panicking).
    fn selected_index(&self) -> Option<usize> {
        let effective = self.effective()?;
        self.options.iter().position(|option| option == effective)
    }

    /// Stages a switch to `value` — see [`Staged::stage`].
    fn stage(&mut self, value: &str) {
        self.value.stage(value);
    }

    /// Whether a switch differing from the current value is pending.
    fn is_changed(&self) -> bool {
        self.value.is_changed()
    }

    /// Discards the pending switch.
    fn reset(&mut self) {
        self.value.reset();
    }

    /// Promotes the pending switch to the current value after a committed Apply.
    fn commit(&mut self) {
        self.value.commit();
    }
}

/// One backing config file kept for surgical editing: its live path and current text.
#[derive(Clone, Debug)]
struct BackingText {
    /// The live XDG path (the [`FileWrite`](crate::core::apply::FileWrite) target; the
    /// writer follows symlinks, R8.5).
    path: PathBuf,
    /// The file's current text, re-parsed on each write so only the targeted value
    /// spans change (the surgical-edit rule, architecture §3).
    text: String,
}

/// Reads a backing config file into a [`BackingText`], or `None` when it is unreadable
/// (missing, permission-revoked, or non-UTF-8) — logged at `debug`.
///
/// A read failure is not surfaced at `warn` here: the section-level gating
/// ([`ThemesModel::themes_editable`]) reports a missing `settings.ini`, and detection
/// already logs the primary gates (R4.4). A missing secondary cursor copy
/// (`hyprland.conf`/`uwsm/env`) just means that copy is skipped, so `debug` is right.
fn read_backing(path: &Path) -> Option<BackingText> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(BackingText {
            path: path.to_path_buf(),
            text,
        }),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "theme backing file unreadable; that copy is skipped");
            None
        }
    }
}

/// The set of backing config files one Theme model edits: each readable file's current
/// text, plus the freshness baseline its conflict check and post-Apply re-baselining need.
///
/// The GTK/icon/cursor model owns four files and the wallpaper model two, but they own
/// them the same way, and duplicating this bookkeeping is how the promotion bug that task
/// 9.6 fixed came to be written twice. One copy means one place to get it right.
///
/// Files are addressed by their live path throughout, which is also what a rendered
/// [`FileWrite`] carries — so the same key identifies a file at load, at lookup, and when
/// its write comes back to be absorbed. A model declares its files once in
/// [`Self::load`] and wraps [`Self::get`] in a named accessor per file, so the call sites
/// still read as "the `gtk-3.0/settings.ini` copy" rather than as an index into a list.
#[derive(Debug)]
struct BackingSet {
    /// The readable files' texts, in the order the model declared them. An unreadable
    /// file is simply absent (R4.4: a missing copy degrades the control, it never fails
    /// the load), which is what makes [`Self::get`] return `Option`.
    files: Vec<BackingText>,
    /// The baseline recorded from the exact bytes read at load, so [`Self::check_conflict`]
    /// catches an external edit (R5.6) and [`Self::absorb_writes`] re-baselines the app's
    /// own write instead of later mistaking it for one. Only readable files are tracked.
    freshness: FreshnessTracker,
}

impl BackingSet {
    /// Reads every path in `paths`, keeping the readable files and baselining them from
    /// the exact bytes read.
    fn load(paths: &[&Path]) -> BackingSet {
        let files: Vec<BackingText> = paths.iter().filter_map(|path| read_backing(path)).collect();
        let mut freshness = FreshnessTracker::new();
        for backing in &files {
            freshness.record_bytes(backing.path.as_path(), backing.text.as_bytes());
        }
        BackingSet { files, freshness }
    }

    /// The current text of the backing file at `path`, or `None` when that file was
    /// unreadable at load.
    fn get(&self, path: &Path) -> Option<&BackingText> {
        self.files.iter().find(|backing| backing.path == path)
    }

    /// Whether any backing file changed on disk since it was loaded (R5.6).
    ///
    /// The Apply glue calls this before writing a dirty change; a `true` result means
    /// another program edited one of the files, so the write must be aborted and the model
    /// reloaded rather than clobbering the stale parse — the same discipline the Display
    /// page follows (the Apply pipeline's own conflict check covers only the store's
    /// files, not these bespoke ones).
    fn check_conflict(&self) -> bool {
        !self.freshness.check_conflicts().is_empty()
    }

    /// Takes the writes a committed Apply performed back into the set: re-baselines each
    /// written file's freshness from the exact bytes written and updates its in-memory
    /// text.
    ///
    /// Re-baselining is what stops the app's own write being mistaken for an external
    /// conflict on the next Apply; updating the text keeps the in-memory copy in step, so
    /// a subsequent edit re-parses the current bytes rather than the pre-Apply ones.
    ///
    /// Writes whose bytes are not UTF-8 are still re-baselined but leave the in-memory
    /// text alone: no parser here emits such bytes, so this only avoids a lossy
    /// conversion.
    ///
    /// A write to a path this set does not hold is re-baselined too — only the *text*
    /// update skips it, since there is no in-memory copy to refresh. That cannot happen
    /// today, because every write is rendered from one of these files; were it ever to,
    /// the stray path would join the freshness baseline and be conflict-checked from then
    /// on, which would widen the write-target scoping task 9.11 deliberately narrowed. Any
    /// future caller passing foreign writes should guard the `record_bytes` call above.
    fn absorb_writes(&mut self, writes: &[FileWrite]) {
        for write in writes {
            self.freshness
                .record_bytes(write.path.as_path(), &write.contents);
            let Ok(text) = std::str::from_utf8(&write.contents) else {
                continue;
            };
            if let Some(backing) = self
                .files
                .iter_mut()
                .find(|backing| backing.path == write.path)
            {
                backing.text = text.to_string();
            }
        }
    }
}
