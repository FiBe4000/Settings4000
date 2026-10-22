//! Small widget builders and layout metrics shared by the category pages.
//!
//! Every page — the declarative framework in [`super::page`] and the bespoke
//! Display, Sound, Theme, Network and Notifications pages — lays itself out the same
//! way: an outer box inset by [`PAGE_MARGIN`], sections and rows stacked
//! [`SECTION_SPACING`] apart, and each setting rendered as a [`labelled_row`] whose
//! parts sit [`ROW_SPACING`] apart, with [`note`] labels for empty or degraded states.
//! Keeping one copy here means the pages cannot drift apart visually (task 9.19).
//!
//! Everything here is plain GTK 4: no libadwaita and no CSS classes or providers, so
//! the widgets inherit the active system GTK theme (R2.1).

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, DropDown, Label, Orientation, StringList, Widget};

/// Outer margin, in pixels, between a page's scroller edge and its content, applied
/// on all four sides.
pub(crate) const PAGE_MARGIN: i32 = 18;

/// Vertical spacing, in pixels, between the sections of a page and between the rows
/// stacked inside a section. The bespoke pages also inset a section's content inside
/// its frame by this amount.
pub(crate) const SECTION_SPACING: i32 = 12;

/// Spacing, in pixels, inside a single row or compound control: between a row's label
/// and its control, and between the parts of a multi-widget control (such as the
/// buttons of a reorderable list).
pub(crate) const ROW_SPACING: i32 = 8;

/// Builds a horizontal settings row: `label` on the left and `control` on the right.
///
/// The label is start-aligned and expands to take all free width, which pushes the
/// control to the row's trailing edge; rows stacked in a column therefore line their
/// labels and their controls up in two clean columns.
pub(crate) fn labelled_row(label: &str, control: &impl IsA<Widget>) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, ROW_SPACING);
    let label = Label::new(Some(label));
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    row.append(&label);
    row.append(control);
    row
}

/// Builds an informational label, used for empty, degraded or explanatory states
/// ("No displays detected.", a staging refusal, a status line).
///
/// The label wraps so a long explanation never forces the window wider than the
/// small sizes R2.4 targets. Both `halign` and `xalign` are set: `halign` places the
/// label widget at the start of its container, while `xalign = 0.0` left-aligns the
/// text *within* the label — a wrapping label can be allocated wider than its text,
/// and GTK's default `xalign` of 0.5 would centre the text in that space.
pub(crate) fn note(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.set_halign(Align::Start);
    label.set_wrap(true);
    label.set_xalign(0.0);
    label
}

/// Builds a `GtkDropDown` over `options`, preselecting index `selected` (when known)
/// and invoking `on_selected` with the chosen option's text on a user change.
///
/// This is the form for drop-downs whose labels *are* the values (a resolution, a
/// theme name). A change to an index outside `options` — GTK reports
/// `GTK_INVALID_LIST_POSITION` when the selection is cleared — is ignored. See
/// [`build_indexed_dropdown`] for the shared construction and selection rules.
pub(crate) fn build_dropdown(
    options: &[String],
    selected: Option<u32>,
    on_selected: impl Fn(String) + 'static,
) -> DropDown {
    let owned = options.to_vec();
    build_indexed_dropdown(options, selected, move |index| {
        if let Some(option) = owned.get(index) {
            on_selected(option.clone());
        }
    })
}

/// Builds a `GtkDropDown` over `labels`, preselecting index `selected` (when known)
/// and invoking `on_selected` with the raw selected index on a user change.
///
/// This is the form for drop-downs whose labels are only a display of some other
/// identity — the Sound page's devices, where the chosen index selects the device's
/// `wpctl` id and two devices may share a label. The index is passed through unchecked
/// (including `GTK_INVALID_LIST_POSITION` as `u32::MAX`), so the caller must look it
/// up with a bounds-checked `get`.
///
/// The drop-down is end-aligned and vertically centred, so it sits at the trailing
/// edge of a [`labelled_row`] without stretching to the row's height. The initial
/// selection is set **before** the change handler is connected, so the programmatic
/// set never fires the handler — a page never mistakes a render for a user edit.
pub(crate) fn build_indexed_dropdown(
    labels: &[String],
    selected: Option<u32>,
    on_selected: impl Fn(usize) + 'static,
) -> DropDown {
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let model = StringList::new(&refs);
    let dropdown = DropDown::builder().model(&model).build();
    dropdown.set_halign(Align::End);
    dropdown.set_valign(Align::Center);
    if let Some(selected) = selected {
        dropdown.set_selected(selected);
    }

    dropdown.connect_selected_notify(move |dropdown| {
        on_selected(dropdown.selected() as usize);
    });
    dropdown
}
