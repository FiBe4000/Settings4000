//! The Theme page's bespoke GTK glue (task 6.3; architecture §7; R2.3, R3.2, R4.2,
//! R4.4).
//!
//! # A multi-section page (6.3–6.4)
//!
//! The Theme page is built from independent **sections** so the later Theme tasks
//! plug in cleanly: task 6.3 adds the palette-scheme section; task 6.4 adds the
//! GTK/icon/cursor theme drop-downs; task 6.5 adds the wallpaper and lock background
//! — each becomes another frame appended in [`Inner::rebuild`], gated on its own
//! model being present. Today the palette section (from a
//! [`PaletteModel`](crate::core::theme::PaletteModel)) and the appearance section
//! (from a [`ThemesModel`](crate::core::theme::ThemesModel)) exist; the page is built
//! whenever *either* model is present, and each section renders only when its model
//! is.
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
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, DrawingArea, DropDown, Frame, Image, Label, Orientation, ScrolledWindow,
    StringList, Widget,
};

use crate::core::theme::{GtkThemeOverrideSource, PaletteModel, Scheme, ThemesModel};

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
    /// Reports a staged change so the window refreshes the Apply/Reset chrome and the
    /// Theme page's dirty marker (task 5.3).
    on_changed: Rc<dyn Fn()>,
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
    /// model is present (the palette source / `gsettings` gates); task 6.5 appends the
    /// wallpaper section here too.
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
}

/// Builds the Theme page over the shared `palette` and `themes` models, reporting
/// staged changes through `on_changed` (tasks 6.3/6.4).
///
/// Either model may be `None` (its section is then not rendered); the window builds the
/// page whenever at least one is present. The returned [`ThemePage`] must be kept alive
/// by the window: it owns the strong reference to the render state whose handlers keep
/// the models wired. The window mounts [`ThemePage::root`] in the stack and calls
/// [`ThemePage::rerender`] after a Reset or a committed Apply.
pub(crate) fn build(
    palette: Rc<RefCell<Option<PaletteModel>>>,
    themes: Rc<RefCell<Option<ThemesModel>>>,
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
