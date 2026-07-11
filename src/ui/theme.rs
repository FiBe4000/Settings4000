//! The Theme page's bespoke GTK glue (tasks 6.3–6.5; architecture §7; R2.3, R3.2,
//! R4.2, R4.4, R8.3).
//!
//! # A multi-section page (6.3–6.5)
//!
//! The Theme page is built from independent **sections** so the Theme tasks plug in
//! cleanly: task 6.3 adds the palette-scheme section; task 6.4 adds the GTK/icon/cursor
//! theme drop-downs; task 6.5 adds the wallpaper and lock-screen background — each is a
//! frame appended in [`Inner::rebuild`], gated on its own model being present. The page
//! is built whenever *any* of the three models exists, and each section renders only
//! when its model is.
//!
//! # The wallpaper section (task 6.5; R8.3)
//!
//! From a [`WallpaperModel`](crate::core::theme::WallpaperModel), the wallpaper section
//! offers a single wallpaper image picker (a plain `gtk::FileDialog` — no libadwaita),
//! a fit-mode drop-down, and an optional "use a different lock-screen image" toggle that
//! reveals a second picker. A chosen path is validated (exists + readable + image
//! extension, R8.3) before staging; an invalid choice is rejected with a plain
//! `gtk::AlertDialog` message and nothing is staged. The lock-override toggle is shown
//! only when hyprlock is present, and the whole section only when hyprpaper is (the
//! window gates it, R4.2/R4.4).
//!
//! # The GTK_THEME override and live restyle (task 6.4; R3.3/R2.2)
//!
//! When a `GTK_THEME` override is in force (set in the app's own environment or
//! uncommented in `uwsm/env`), the appearance section shows a plain-GTK banner (a
//! framed warning-icon-plus-label row — no libadwaita, no custom CSS) and disables
//! the GTK-theme drop-down, so the app never fights the override. Whether the section
//! claims a live GTK-theme restyle or "takes effect at next launch" is gated on the
//! model's [`live_restyle`](crate::core::theme::ThemesModel::live_restyle) flag
//! (R2.2). The whole appearance section is only built when `gsettings` is present
//! (the window gates it, R4.2).
//!
//! # Why this page is bespoke, not declarative
//!
//! The file-backed §6 pages are a list of fixed
//! [`RowDescriptor`](super::row::RowDescriptor)s rendered by the declarative
//! framework, which stages edits into the [`SettingsStore`](crate::core::store). The
//! palette scheme fits neither: it is not a `SettingId`-keyed store value, and its
//! Apply runs `generate-colors` rather than writing a file (see
//! [`crate::core::theme`]). So — mirroring the Display ([`super::display`]) and Sound
//! ([`super::sound`]) pages — it renders directly from the GTK-free
//! [`PaletteModel`](crate::core::theme::PaletteModel), a second staging source the
//! window folds into the shared Apply/Reset chrome.
//!
//! # Render-from-the-model, rebuild-on-change
//!
//! The widgets are thin: choosing a scheme stages it in the model, reports the edit
//! through the shared `on_changed` callback (so the window's Apply/Reset chrome and
//! the Theme page's dirty marker light up), and rebuilds the section from the model.
//! Each rebuild constructs a fresh drop-down and sets its selection *before*
//! connecting the change handler, so a programmatic render never masquerades as a
//! user edit — the same discipline the Display and Sound pages follow. The window
//! drives a rebuild after a Reset or a committed Apply through
//! [`ThemePage::rerender`].
//!
//! # The read-only degrade and the swatch (R3.2, task 3.7)
//!
//! With fewer than two schemes there is nothing to switch to, so the section shows
//! the active scheme as read-only text instead of a drop-down
//! ([`PaletteModel::is_switchable`](crate::core::theme::PaletteModel::is_switchable)).
//! When a drop-down is shown, a small preview strip beneath it draws the selected
//! scheme's colors with a [`DrawingArea`] + Cairo — deliberately *not* CSS-colored
//! widgets, which would trip the no-custom-CSS guard (`tests/no_custom_css.rs`); a
//! Cairo draw function paints the rectangles directly.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use gtk4::gio;
use gtk4::prelude::*;
use gtk4::{
    AlertDialog, Align, Box as GtkBox, Button, DrawingArea, DropDown, FileDialog, FileFilter,
    Frame, Image, Label, Orientation, ScrolledWindow, StringList, Switch, Widget, Window,
};

use crate::core::theme::{
    GtkThemeOverrideSource, PaletteModel, Scheme, ThemesModel, WallpaperModel,
};

/// Outer margin, in pixels, around the page content.
const PAGE_MARGIN: i32 = 18;

/// Vertical spacing, in pixels, between sections and between rows.
const SECTION_SPACING: i32 = 12;

/// Horizontal spacing, in pixels, between a row label and its control.
const ROW_SPACING: i32 = 8;

/// Height, in pixels, of the selected scheme's color preview strip.
const SWATCH_HEIGHT: i32 = 16;

/// The mounted Theme page: the scrollable root plus the handle the window uses to
/// re-render it after a Reset or a committed Apply.
pub(crate) struct ThemePage {
    /// The scrollable widget mounted in the window's stack.
    root: ScrolledWindow,
    /// The shared render state; kept alive for the life of the page (its controls'
    /// handlers hold only [`std::rc::Weak`] references, so this strong reference is
    /// what keeps the state — and thus the handlers — alive).
    inner: Rc<Inner>,
}

impl ThemePage {
    /// The widget to add to the window's stack.
    pub(crate) fn root(&self) -> &ScrolledWindow {
        &self.root
    }

    /// Rebuilds the sections from the current model — called by the window after a
    /// Reset or a committed Apply so the palette drop-down snaps to the model's values.
    pub(crate) fn rerender(&self) {
        self.inner.rebuild();
    }
}

/// The shared render state the page's control handlers operate on.
///
/// Handlers capture a [`std::rc::Weak`] to this and upgrade on use, so the widget tree
/// (owned via `content`) never forms a reference cycle with the closures mounted
/// inside it.
struct Inner {
    /// The vertical box holding one frame per section, rebuilt in place.
    content: GtkBox,
    /// The shared palette-scheme model — `None` when there is no palette source (the
    /// palette section is then not built). Shared with the window so an Apply reads the
    /// same staged scheme.
    palette: Rc<RefCell<Option<PaletteModel>>>,
    /// The shared GTK/icon/cursor theme model (task 6.4) — `None` when `gsettings` is
    /// absent (the appearance section is then not built). Shared with the window so an
    /// Apply reads the same staged theme/cursor values.
    themes: Rc<RefCell<Option<ThemesModel>>>,
    /// The shared wallpaper / lock-background model (task 6.5) — `None` when hyprpaper is
    /// absent (the wallpaper section is then not built). Shared with the window so an
    /// Apply reads the same staged wallpaper/lock values.
    wallpaper: Rc<RefCell<Option<WallpaperModel>>>,
    /// Reports a staged change so the window refreshes the Apply/Reset chrome and the
    /// Theme page's dirty marker (task 5.3).
    on_changed: Rc<dyn Fn()>,
}

/// Which wallpaper image picker was invoked, so the file-chooser callback stages the
/// chosen path into the right [`WallpaperModel`] setter (task 6.5).
#[derive(Clone, Copy)]
enum WallpaperTarget {
    /// The desktop wallpaper (`hyprpaper.conf`).
    Wallpaper,
    /// The lock-screen background override (`hyprlock.conf`).
    Lock,
}

/// Which theme drop-down reported a change, so [`Inner::stage_theme`] routes it to the
/// right [`ThemesModel`] setter.
#[derive(Clone, Copy)]
enum ThemeKind {
    /// The GTK theme drop-down.
    Gtk,
    /// The icon theme drop-down.
    Icon,
    /// The cursor theme drop-down.
    Cursor,
    /// The cursor size drop-down.
    CursorSize,
}

impl Inner {
    /// Rebuilds every section from the models. Each section is appended only when its
    /// model is present (the palette source / `gsettings` / hyprpaper gates).
    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        if self.palette.borrow().is_some() {
            self.content.append(&self.build_palette_section());
        }
        if self.themes.borrow().is_some() {
            self.content.append(&self.build_themes_section());
        }
        if self.wallpaper.borrow().is_some() {
            self.content.append(&self.build_wallpaper_section());
        }
    }

    /// Builds the palette-scheme section: a scheme drop-down with a preview strip, or a
    /// read-only display of the active scheme when there are fewer than two schemes
    /// (R3.2).
    fn build_palette_section(self: &Rc<Self>) -> Frame {
        let frame = Frame::new(Some("Colour palette"));
        let section = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
        section.set_margin_top(SECTION_SPACING);
        section.set_margin_bottom(SECTION_SPACING);
        section.set_margin_start(SECTION_SPACING);
        section.set_margin_end(SECTION_SPACING);

        let model_ref = self.palette.borrow();
        let Some(model) = model_ref.as_ref() else {
            // The window only builds this page when a palette source exists, so this is
            // a defensive fallback rather than an expected state.
            section.append(&note("No colour palette source was found."));
            frame.set_child(Some(&section));
            return frame;
        };

        if model.is_switchable() {
            // The scheme drop-down: choosing an entry stages a switch. The active (or
            // pending) scheme is preselected.
            let names: Vec<String> = model
                .schemes()
                .iter()
                .map(|s| s.name().to_string())
                .collect();
            let selected = model.selected_index().map(|index| index as u32);
            let weak = Rc::downgrade(self);
            let dropdown = build_dropdown(&names, selected, move |name| {
                if let Some(inner) = weak.upgrade() {
                    inner.stage_scheme(name);
                }
            });
            section.append(&labelled_row("Scheme", &dropdown));

            // A preview strip of the selected scheme's colors (task 3.7 swatch).
            if let Some(index) = model.selected_index() {
                let preview = model.schemes()[index].preview();
                if !preview.is_empty() {
                    section.append(&swatch_strip(preview));
                }
            } else {
                // The active scheme could not be matched to an enumerated scheme —
                // either its generated header degraded to unknown or it names a
                // scheme absent from `colors/` (see `PaletteModel::selected_index`).
                // GTK silently defaults an unset drop-down selection to index 0,
                // which would misleadingly present the first scheme as the active
                // one; this note tells the user the current scheme is indeterminate
                // so the index-0 default is not mistaken for it (task 6.3 review S2).
                section.append(&note("Current colour scheme could not be determined."));
            }
        } else {
            // R3.2: fewer than two schemes — show the active scheme read-only.
            let active = model
                .active()
                .or_else(|| model.schemes().first().map(Scheme::name))
                .unwrap_or("unknown");
            section.append(&note(&format!("Active colour scheme: {active}")));
            // The explanation depends on how many schemes were found: zero valid
            // schemes in `colors/` is a different situation from a single scheme with
            // nothing to switch to, and the "only one is available" wording would be
            // wrong for the empty case.
            let explanation = if model.schemes().is_empty() {
                "No colour schemes were found."
            } else {
                "Only one colour scheme is available, so there is nothing to switch to."
            };
            section.append(&note(explanation));
        }

        frame.set_child(Some(&section));
        frame
    }

    /// Stages a switch to `name`, notifies the chrome, and rebuilds the section.
    ///
    /// The mutable model borrow is released before `on_changed` runs (which re-reads
    /// the model to derive the chrome) and before the rebuild re-reads it.
    fn stage_scheme(self: &Rc<Self>, name: String) {
        {
            let mut palette = self.palette.borrow_mut();
            if let Some(model) = palette.as_mut() {
                model.stage(&name);
            }
        }
        (self.on_changed)();
        self.rebuild();
    }

    /// Builds the GTK/icon/cursor appearance section: the GTK-theme, icon-theme,
    /// cursor-theme, and cursor-size drop-downs, preselected from the backing config,
    /// with the `GTK_THEME` override banner and the live-restyle note (task 6.4).
    ///
    /// When `settings.ini` is unreadable the rows are hidden behind a note (R4.4); when
    /// a `GTK_THEME` override is active the GTK-theme drop-down is disabled and the
    /// banner shown, but icon/cursor stay editable (R3.3).
    fn build_themes_section(self: &Rc<Self>) -> Frame {
        let frame = Frame::new(Some("Application appearance"));
        let section = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
        section.set_margin_top(SECTION_SPACING);
        section.set_margin_bottom(SECTION_SPACING);
        section.set_margin_start(SECTION_SPACING);
        section.set_margin_end(SECTION_SPACING);

        let model_ref = self.themes.borrow();
        let Some(model) = model_ref.as_ref() else {
            // The window only builds this page's appearance section when a themes model
            // exists, so this is a defensive fallback rather than an expected state.
            section.append(&note("No theme settings are available."));
            frame.set_child(Some(&section));
            return frame;
        };

        if !model.themes_editable() {
            // R4.4: no readable settings.ini, so there is nothing to preselect or write.
            section.append(&note(
                "GTK settings.ini was not found, so theme controls are unavailable.",
            ));
            frame.set_child(Some(&section));
            return frame;
        }

        // R3.3: a live GTK_THEME override — show a banner and disable the GTK drop-down.
        if let Some(source) = model.gtk_override() {
            section.append(&override_banner(source));
        }

        // GTK theme (disabled under an override).
        let gtk_dropdown = self.theme_dropdown(
            model.gtk_themes(),
            model.selected_gtk_index(),
            ThemeKind::Gtk,
        );
        gtk_dropdown.set_sensitive(!model.gtk_dropdown_disabled());
        section.append(&labelled_row("GTK theme", &gtk_dropdown));

        // R2.2: whether a GTK theme change restyles live or takes effect next launch.
        let restyle = if model.live_restyle() {
            "Changing the GTK theme restyles running apps immediately."
        } else {
            "GTK theme changes take effect the next time each app starts."
        };
        section.append(&note(restyle));

        // Icon theme.
        let icon_dropdown = self.theme_dropdown(
            model.icon_themes(),
            model.selected_icon_index(),
            ThemeKind::Icon,
        );
        section.append(&labelled_row("Icon theme", &icon_dropdown));

        // Cursor theme.
        let cursor_dropdown = self.theme_dropdown(
            model.cursor_themes(),
            model.selected_cursor_index(),
            ThemeKind::Cursor,
        );
        section.append(&labelled_row("Cursor theme", &cursor_dropdown));

        // Cursor size.
        let size_dropdown = self.theme_dropdown(
            model.cursor_sizes(),
            model.selected_cursor_size_index(),
            ThemeKind::CursorSize,
        );
        section.append(&labelled_row("Cursor size", &size_dropdown));

        frame.set_child(Some(&section));
        frame
    }

    /// Builds a drop-down over `options` preselecting `selected`, staging a change of
    /// `kind` when the user picks a different entry.
    fn theme_dropdown(
        self: &Rc<Self>,
        options: &[String],
        selected: Option<usize>,
        kind: ThemeKind,
    ) -> DropDown {
        let names = options.to_vec();
        let weak = Rc::downgrade(self);
        build_dropdown(&names, selected.map(|index| index as u32), move |value| {
            if let Some(inner) = weak.upgrade() {
                inner.stage_theme(kind, value);
            }
        })
    }

    /// Stages a theme/cursor change, notifies the chrome, and rebuilds the sections.
    ///
    /// The mutable model borrow is released before `on_changed` runs (which re-reads
    /// the model to derive the chrome) and before the rebuild re-reads it.
    fn stage_theme(self: &Rc<Self>, kind: ThemeKind, value: String) {
        {
            let mut themes = self.themes.borrow_mut();
            if let Some(model) = themes.as_mut() {
                match kind {
                    ThemeKind::Gtk => model.stage_gtk_theme(&value),
                    ThemeKind::Icon => model.stage_icon_theme(&value),
                    ThemeKind::Cursor => model.stage_cursor_theme(&value),
                    ThemeKind::CursorSize => model.stage_cursor_size(&value),
                }
            }
        }
        (self.on_changed)();
        self.rebuild();
    }

    /// Builds the wallpaper section: a wallpaper image picker, a fit-mode drop-down, and
    /// an optional lock-screen override toggle that reveals a second picker (task 6.5).
    ///
    /// When `hyprpaper.conf` is unreadable the rows are hidden behind a note (R4.4); the
    /// lock-override toggle is shown only when hyprlock is present (R4.2).
    fn build_wallpaper_section(self: &Rc<Self>) -> Frame {
        let frame = Frame::new(Some("Wallpaper"));
        let section = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
        section.set_margin_top(SECTION_SPACING);
        section.set_margin_bottom(SECTION_SPACING);
        section.set_margin_start(SECTION_SPACING);
        section.set_margin_end(SECTION_SPACING);

        let model_ref = self.wallpaper.borrow();
        let Some(model) = model_ref.as_ref() else {
            // The window only builds this section when a wallpaper model exists, so this
            // is a defensive fallback rather than an expected state.
            section.append(&note("No wallpaper settings are available."));
            frame.set_child(Some(&section));
            return frame;
        };

        if !model.wallpaper_editable() {
            // R4.4: no readable hyprpaper.conf, so there is nothing to preselect or write.
            section.append(&note(
                "hyprpaper.conf was not found, so the wallpaper cannot be changed here.",
            ));
            frame.set_child(Some(&section));
            return frame;
        }

        // The wallpaper image picker.
        let button = self.image_chooser_button(model.wallpaper_path(), WallpaperTarget::Wallpaper);
        section.append(&labelled_row("Image", &button));

        // The fit-mode drop-down.
        let weak = Rc::downgrade(self);
        let fit_dropdown = build_dropdown(
            model.fit_options(),
            model.selected_fit_index().map(|index| index as u32),
            move |fit| {
                if let Some(inner) = weak.upgrade() {
                    inner.stage_fit(fit);
                }
            },
        );
        section.append(&labelled_row("Fit", &fit_dropdown));

        // The optional lock-screen override — only when hyprlock is present (R4.2).
        if model.lock_editable() {
            let switch = Switch::new();
            switch.set_halign(Align::End);
            switch.set_valign(Align::Center);
            // Set the state before connecting the handler so the programmatic set never
            // masquerades as a user toggle (the same discipline as the drop-downs).
            switch.set_active(model.override_on());
            let weak = Rc::downgrade(self);
            switch.connect_active_notify(move |switch| {
                if let Some(inner) = weak.upgrade() {
                    inner.set_override(switch.is_active());
                }
            });
            section.append(&labelled_row("Use a different lock-screen image", &switch));

            if model.override_on() {
                let lock_button =
                    self.image_chooser_button(model.lock_path(), WallpaperTarget::Lock);
                section.append(&labelled_row("Lock-screen image", &lock_button));
            }
        }

        frame.set_child(Some(&section));
        frame
    }

    /// Builds a button showing the current image's file name (or "Choose…") that opens a
    /// file chooser for `target` on click; the full path is the tooltip (task 6.5).
    fn image_chooser_button(
        self: &Rc<Self>,
        current: Option<&str>,
        target: WallpaperTarget,
    ) -> Button {
        let label = current
            .map(basename)
            .unwrap_or_else(|| "Choose…".to_string());
        let button = Button::with_label(&label);
        button.set_halign(Align::End);
        if let Some(path) = current {
            button.set_tooltip_text(Some(path));
        }
        let weak = Rc::downgrade(self);
        let initial = current.map(str::to_string);
        button.connect_clicked(move |button| {
            if let Some(inner) = weak.upgrade() {
                inner.choose_image(button, target, initial.clone());
            }
        });
        button
    }

    /// Opens a plain `gtk::FileDialog` filtered to image files and stages the chosen
    /// path into `target` (task 6.5).
    ///
    /// The dialog is asynchronous: its callback runs later on the main thread, upgrades
    /// the captured [`Weak`](std::rc::Weak), and stages (with validation) the chosen
    /// path. A dismissed dialog is not an error. No libadwaita is involved.
    fn choose_image(
        self: &Rc<Self>,
        button: &Button,
        target: WallpaperTarget,
        initial: Option<String>,
    ) {
        let dialog = FileDialog::builder()
            .title("Choose an image")
            .modal(true)
            .build();
        if let Some(path) = &initial {
            dialog.set_initial_file(Some(&gio::File::for_path(path)));
        }
        // Offer an image-only filter (the same extensions the validator accepts).
        let filter = FileFilter::new();
        filter.set_name(Some("Images"));
        for suffix in ["png", "jpg", "jpeg", "webp"] {
            filter.add_suffix(suffix);
        }
        let filters = gio::ListStore::new::<FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));

        let parent = button.root().and_downcast::<Window>();
        let weak = Rc::downgrade(self);
        dialog.open(parent.as_ref(), gio::Cancellable::NONE, move |result| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            match result {
                Ok(file) => {
                    if let Some(path) = file.path() {
                        inner.apply_chosen_image(target, &path.to_string_lossy());
                    }
                }
                Err(error) => {
                    // A dismissed/cancelled dialog reports an error; it is not worth
                    // surfacing to the user.
                    tracing::debug!(%error, "image chooser dismissed");
                }
            }
        });
    }

    /// Stages a chosen image path into the model, validating it first (R8.3); on
    /// rejection shows a plain-GTK error dialog and stages nothing.
    fn apply_chosen_image(self: &Rc<Self>, target: WallpaperTarget, path: &str) {
        let result = {
            let mut wallpaper = self.wallpaper.borrow_mut();
            match wallpaper.as_mut() {
                Some(model) => match target {
                    WallpaperTarget::Wallpaper => model.stage_wallpaper(path),
                    WallpaperTarget::Lock => model.stage_lock(path),
                },
                None => Ok(()),
            }
        };
        match result {
            Ok(()) => {
                (self.on_changed)();
                self.rebuild();
            }
            Err(error) => {
                // R8.3: reject a missing / unreadable / non-image path with a clear
                // message, leaving the previously staged value unchanged.
                self.show_image_error(&error.to_string());
            }
        }
    }

    /// Stages a fit mode, notifies the chrome, and rebuilds the sections.
    fn stage_fit(self: &Rc<Self>, fit: String) {
        {
            let mut wallpaper = self.wallpaper.borrow_mut();
            if let Some(model) = wallpaper.as_mut() {
                model.stage_fit(&fit);
            }
        }
        (self.on_changed)();
        self.rebuild();
    }

    /// Sets the lock-screen override toggle, notifies the chrome, and rebuilds so the
    /// override picker appears or disappears.
    fn set_override(self: &Rc<Self>, on: bool) {
        {
            let mut wallpaper = self.wallpaper.borrow_mut();
            if let Some(model) = wallpaper.as_mut() {
                model.set_override(on);
            }
        }
        (self.on_changed)();
        self.rebuild();
    }

    /// Shows a plain `gtk::AlertDialog` explaining why a chosen image was rejected
    /// (R8.3) — no libadwaita, no custom CSS.
    fn show_image_error(&self, detail: &str) {
        let dialog = AlertDialog::builder()
            .message("That image can't be used")
            .detail(detail)
            .modal(true)
            .build();
        let parent = self.content.root().and_downcast::<Window>();
        dialog.show(parent.as_ref());
    }
}

/// Builds the Theme page over the shared `palette`, `themes`, and `wallpaper` models,
/// reporting staged changes through `on_changed` (tasks 6.3/6.4/6.5).
///
/// Any model may be `None` (its section is then not rendered); the window builds the
/// page whenever at least one is present. The returned [`ThemePage`] must be kept alive
/// by the window: it owns the strong reference to the render state whose handlers keep
/// the models wired. The window mounts [`ThemePage::root`] in the stack and calls
/// [`ThemePage::rerender`] after a Reset or a committed Apply.
pub(crate) fn build(
    palette: Rc<RefCell<Option<PaletteModel>>>,
    themes: Rc<RefCell<Option<ThemesModel>>>,
    wallpaper: Rc<RefCell<Option<WallpaperModel>>>,
    on_changed: Rc<dyn Fn()>,
) -> ThemePage {
    let content = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    content.set_margin_top(PAGE_MARGIN);
    content.set_margin_bottom(PAGE_MARGIN);
    content.set_margin_start(PAGE_MARGIN);
    content.set_margin_end(PAGE_MARGIN);

    let inner = Rc::new(Inner {
        content: content.clone(),
        palette,
        themes,
        wallpaper,
        on_changed,
    });
    inner.rebuild();

    let root = ScrolledWindow::new();
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.set_child(Some(&content));

    ThemePage { root, inner }
}

/// A horizontal strip of equal-width color rectangles, one per preview color, drawn
/// with Cairo (task 3.7 swatch).
///
/// A [`DrawingArea`] with a Cairo draw function is used rather than CSS-colored widgets
/// because the app ships no custom CSS (R2.1) — the `tests/no_custom_css.rs` guard
/// forbids a `CssProvider`, and there is no non-CSS way to set a widget's background
/// color. Cairo paints the rectangles directly, so no styling is needed. The last
/// rectangle is widened by rounding up so no sub-pixel gap shows at the right edge.
fn swatch_strip(colors: &[(f64, f64, f64)]) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_content_height(SWATCH_HEIGHT);
    area.set_hexpand(true);

    let colors = colors.to_vec();
    area.set_draw_func(move |_, cr, width, height| {
        let count = colors.len();
        if count == 0 {
            return;
        }
        let swatch_width = f64::from(width) / count as f64;
        for (index, (r, g, b)) in colors.iter().enumerate() {
            cr.rectangle(
                index as f64 * swatch_width,
                0.0,
                swatch_width.ceil(),
                f64::from(height),
            );
            cr.set_source_rgb(*r, *g, *b);
            // A fill can only fail on an already-errored Cairo context, which does not
            // occur for a fresh draw; ignore the result rather than panic in a render.
            let _ = cr.fill();
        }
    });
    area
}

/// Builds a `GtkDropDown` over `options`, preselecting `selected` (when known) and
/// invoking `on_selected` with the chosen option on a user change.
///
/// The initial selection is set **before** the change handler is connected, so the
/// programmatic set never fires the handler — the page never mistakes a render for a
/// user edit.
fn build_dropdown(
    options: &[String],
    selected: Option<u32>,
    on_selected: impl Fn(String) + 'static,
) -> DropDown {
    let labels: Vec<&str> = options.iter().map(String::as_str).collect();
    let model = StringList::new(&labels);
    let dropdown = DropDown::builder().model(&model).build();
    dropdown.set_halign(Align::End);
    dropdown.set_valign(Align::Center);
    if let Some(selected) = selected {
        dropdown.set_selected(selected);
    }

    let options = options.to_vec();
    dropdown.connect_selected_notify(move |dropdown| {
        if let Some(option) = options.get(dropdown.selected() as usize) {
            on_selected(option.clone());
        }
    });
    dropdown
}

/// A left-aligned row: a `label` taking the free space and its `control` on the right.
fn labelled_row(label: &str, control: &impl IsA<Widget>) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, ROW_SPACING);
    let label = Label::new(Some(label));
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    row.append(&label);
    row.append(control);
    row
}

/// A left-aligned, wrapping informational label for an empty/degraded state.
fn note(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.set_halign(Align::Start);
    label.set_wrap(true);
    label.set_xalign(0.0);
    label
}

/// The file name of a path, for a compact chooser-button label. Falls back to the whole
/// path when it has no final component (which a real image path always does).
fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string())
}

/// A plain-GTK banner explaining an active `GTK_THEME` override (R3.3).
///
/// A [`Frame`] wrapping a warning icon and a wrapping label — deliberately *not*
/// libadwaita's `Banner` and with no custom CSS (it uses only stock widgets and the
/// system theme's `dialog-warning-symbolic` icon), so the `tests/no_custom_css.rs`
/// guard still passes. It tells the user the GTK theme is forced and cannot be changed
/// here, and where the override comes from, so the disabled drop-down is not mysterious.
fn override_banner(source: &GtkThemeOverrideSource) -> Frame {
    let frame = Frame::new(None);
    let row = GtkBox::new(Orientation::Horizontal, ROW_SPACING);
    row.set_margin_top(ROW_SPACING);
    row.set_margin_bottom(ROW_SPACING);
    row.set_margin_start(ROW_SPACING);
    row.set_margin_end(ROW_SPACING);

    let icon = Image::from_icon_name("dialog-warning-symbolic");
    icon.set_valign(Align::Start);
    row.append(&icon);

    let label = Label::new(Some(&source.banner_message()));
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    label.set_wrap(true);
    label.set_xalign(0.0);
    row.append(&label);

    frame.set_child(Some(&row));
    frame
}
