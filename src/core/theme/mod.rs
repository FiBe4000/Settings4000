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
//! this file (`Selection`, `PathField`, `BackingText`, `read_backing` — private, hence
//! named rather than linked). Each submodule's own module docs, and the docs on each model
//! type, carry the detail — the list above exists only so a reader lands in the right
//! submodule. The models are re-exported here, so every caller keeps addressing them as
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

mod palette;
mod themes;
mod wallpaper;

pub use palette::{PaletteModel, Scheme};
pub use themes::{GtkThemeOverrideSource, ThemeRoots, ThemesApply, ThemesModel, ThemesPaths};
pub use wallpaper::{WallpaperApply, WallpaperModel, WallpaperPaths};

/// One drop-down's staged selection: the discovered options, the current value read
/// from the backing config, and any pending switch.
///
/// Mirrors the store's `original`/`staged` dirty rule (re-selecting the current value
/// clears the pending switch, so it never lights up Apply). Used for all four theme
/// controls; the cursor size holds its numeric value as a string so the same logic
/// serves it (parsed to an integer only when written/reloaded).
#[derive(Clone, Debug)]
struct Selection {
    /// The drop-down's candidate values, in display order. Always includes the current
    /// value (prepended when discovery did not surface it) so it stays selectable.
    options: Vec<String>,
    /// The value read from the backing config, or `None` when the config did not set
    /// it. Selecting any value while this is `None` counts as a change (a write that
    /// appends the key).
    original: Option<String>,
    /// The pending selection, or `None` when nothing is staged. Only ever set to a
    /// value that differs from [`original`](Self::original), so `staged.is_some()` is
    /// exactly the dirty condition.
    staged: Option<String>,
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
            original,
            staged: None,
        }
    }

    /// The effective value — the staged selection if pending, else the current value.
    fn effective(&self) -> Option<&str> {
        self.staged.as_deref().or(self.original.as_deref())
    }

    /// The index of the effective value within [`options`](Self::options), for
    /// preselecting the drop-down. `None` when the effective value is not among the
    /// options (which cannot happen once [`new`](Self::new) has made `original`
    /// selectable, but is handled without panicking).
    fn selected_index(&self) -> Option<usize> {
        let effective = self.effective()?;
        self.options.iter().position(|option| option == effective)
    }

    /// Stages a switch to `value`, clearing the pending switch when it equals the
    /// current value (so re-selecting the current value is not dirty).
    fn stage(&mut self, value: &str) {
        if self.original.as_deref() == Some(value) {
            self.staged = None;
        } else {
            self.staged = Some(value.to_string());
        }
    }

    /// Whether a switch differing from the current value is pending.
    fn is_changed(&self) -> bool {
        self.staged.is_some()
    }

    /// Discards the pending switch.
    fn reset(&mut self) {
        self.staged = None;
    }

    /// Promotes the pending switch to the current value after a committed Apply.
    fn commit(&mut self) {
        if let Some(value) = self.staged.take() {
            self.original = Some(value);
        }
    }
}

/// A free-form path setting with an original (on-disk) value and an optional staged
/// edit.
///
/// Mirrors [`Selection`]'s dirty rule for a value that is *not* a fixed drop-down set:
/// re-staging the current value clears the pending edit, so it never lights up Apply.
/// Used for the wallpaper and lock-screen image paths, which the user picks with a file
/// chooser rather than from a list.
#[derive(Clone, Debug)]
struct PathField {
    /// The path read from the backing config, or `None` when the config did not set it.
    original: Option<String>,
    /// The pending path, or `None` when nothing is staged. Only ever set to a value
    /// that differs from [`original`](Self::original).
    staged: Option<String>,
}

impl PathField {
    /// Builds a path field over a current value.
    fn new(original: Option<String>) -> Self {
        PathField {
            original,
            staged: None,
        }
    }

    /// The effective value — the staged path if pending, else the current one.
    fn effective(&self) -> Option<&str> {
        self.staged.as_deref().or(self.original.as_deref())
    }

    /// Stages `value`, clearing the pending edit when it equals the current value.
    fn stage(&mut self, value: &str) {
        if self.original.as_deref() == Some(value) {
            self.staged = None;
        } else {
            self.staged = Some(value.to_string());
        }
    }

    /// Whether a pending edit differing from the current value exists.
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
