//! The Theme page's bespoke GTK glue (task 6.3; architecture §7; R2.3, R3.2, R4.2,
//! R4.4).
//!
//! # A multi-section page (6.3 adds the first section)
//!
//! The Theme page is built from independent **sections** so the later Theme tasks
//! plug in cleanly: task 6.3 adds the palette-scheme section here; task 6.4 adds
//! GTK/icon/cursor theme drop-downs and task 6.5 adds the wallpaper and lock
//! background — each becomes another frame appended in [`Inner::rebuild`], gated on
//! its own capability. Today only the palette section exists.
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
    Align, Box as GtkBox, DrawingArea, DropDown, Frame, Label, Orientation, ScrolledWindow,
    StringList, Widget,
};

use crate::core::theme::{PaletteModel, Scheme};

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
    /// The shared palette-scheme model — `None` only when there is no palette source
    /// (the window then shows a placeholder instead of building this page). Shared with
    /// the window so an Apply reads the same staged scheme.
    palette: Rc<RefCell<Option<PaletteModel>>>,
    /// Reports a staged switch so the window refreshes the Apply/Reset chrome and the
    /// Theme page's dirty marker (task 5.3).
    on_changed: Rc<dyn Fn()>,
}

impl Inner {
    /// Rebuilds every section from the model. Tasks 6.4/6.5 append their sections here.
    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        self.content.append(&self.build_palette_section());
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
}

/// Builds the Theme page over the shared palette `model`, reporting staged switches
/// through `on_changed` (task 6.3).
///
/// The returned [`ThemePage`] must be kept alive by the window: it owns the strong
/// reference to the render state whose handlers keep the model wired. The window mounts
/// [`ThemePage::root`] in the stack and calls [`ThemePage::rerender`] after a Reset or a
/// committed Apply.
pub(crate) fn build(
    palette: Rc<RefCell<Option<PaletteModel>>>,
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
