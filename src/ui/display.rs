//! The Display page's bespoke GTK glue (task 6.1; architecture §7; R2.3, R4.2,
//! R4.4, R5.2).
//!
//! # Why this page is bespoke, not declarative
//!
//! Every other §6 page is a list of fixed [`RowDescriptor`](super::row::RowDescriptor)s
//! rendered by the declarative framework ([`super::page`]). The Display page cannot
//! use it for two reasons: its controls are **per-monitor and discovered at runtime**
//! (the descriptor framework is a fixed list keyed by the fieldless
//! [`SettingId`](crate::core::model::SettingId)), and the laptop enable control is
//! **runtime-only** (R5.2) — the framework renders from `store.value(id)` and
//! `debug_assert!`s file-backed settings, so a runtime control has no place there.
//! This module therefore renders the page directly from the GTK-free
//! [`DisplayModel`](crate::core::display::DisplayModel), which owns the per-monitor
//! staging and the runtime toggle.
//!
//! # Render-from-the-model, rebuild-on-change
//!
//! Like the rest of the UI the widgets are thin: a control's change mutates the
//! model and the page then **rebuilds** its widget tree from the model, so the model
//! is the single source of truth (the same discipline [`super::page`] follows with
//! its store round-trip). Rebuilding on each change is what lets a dependent drop-down
//! update — the refresh-rate options depend on the chosen resolution, and the
//! mode/scale/position controls appear only while a monitor is enabled. Because each
//! rebuild constructs fresh widgets and sets their value *before* connecting the
//! change handler, a programmatic set never masquerades as a user edit (the feedback
//! gotcha [`super::page`] solves with signal-blocking is avoided here by construction).
//!
//! The window drives a rebuild after a Reset or a committed Apply through
//! [`DisplayPage::rerender`], mirroring the `PageMsg::Rerender` broadcast the
//! declarative pages receive.
//!
//! # Staged vs. runtime controls (R5.1/R5.2)
//!
//! A non-laptop monitor's resolution/refresh/scale/position/enable edits are **staged**
//! into the model and written to `monitors.conf` on Apply; the page reports each
//! through the shared `on_changed` callback so the window's Apply/Reset chrome lights
//! up (as the declarative pages do). The laptop panel's enable control is
//! **runtime-only**: toggling it calls [`DisplayModel::toggle_laptop`] immediately
//! (writing the hotplug state file + a live `hyprctl reload`), never staging and never
//! marking the page dirty. Its mode/scale, by contrast, are staged like any monitor's
//! (the single-source gotcha, analysis §6.2).

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, DropDown, Frame, Label, Orientation, ScrolledWindow, StringList, Switch,
    Widget,
};

use crate::core::display::DisplayModel;
use crate::system::command::SystemCommandRunner;

/// Outer margin, in pixels, around the page content.
const PAGE_MARGIN: i32 = 18;

/// Vertical spacing, in pixels, between monitor sections and between rows.
const SECTION_SPACING: i32 = 12;

/// Horizontal spacing, in pixels, between a row label and its control.
const ROW_SPACING: i32 = 8;

/// The mounted Display page: the scrollable root plus the handle the window uses to
/// re-render it after a Reset or a committed Apply.
pub(crate) struct DisplayPage {
    /// The scrollable widget mounted in the window's stack.
    root: ScrolledWindow,
    /// The shared render state; kept alive for the life of the page (its controls'
    /// handlers hold only [`Weak`] references, so this strong reference is what keeps
    /// the state — and thus the handlers — alive).
    inner: Rc<Inner>,
}

impl DisplayPage {
    /// The widget to add to the window's stack.
    pub(crate) fn root(&self) -> &ScrolledWindow {
        &self.root
    }

    /// Rebuilds the controls from the current model — called by the window after a
    /// Reset or a committed Apply so the drop-downs snap to the model's values.
    pub(crate) fn rerender(&self) {
        self.inner.rebuild();
    }
}

/// The shared render state the page's control handlers operate on.
///
/// Handlers capture a [`Weak`] to this and upgrade on use, so the widget tree (owned
/// via `content`) never forms a reference cycle with the closures mounted inside it.
struct Inner {
    /// The vertical box holding one section per monitor, rebuilt in place.
    content: GtkBox,
    /// The shared Display model — `None` until the startup worker builds it (or when
    /// there is no live compositor). Shared with the window so an Apply reads the same
    /// staged edits.
    model: Rc<RefCell<Option<DisplayModel>>>,
    /// Reports a staged edit so the window refreshes the Apply/Reset chrome and the
    /// Display page's dirty marker (task 5.3). Not called for the runtime laptop
    /// toggle, which never changes dirty state.
    on_changed: Rc<dyn Fn()>,
}

impl Inner {
    /// Rebuilds every monitor section from the model (R2.3/R4.2/R4.4).
    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }

        let model_ref = self.model.borrow();
        let Some(model) = model_ref.as_ref() else {
            // No model (no live compositor): a plain note, no controls.
            self.content.append(&note("No displays detected."));
            return;
        };

        if model.monitor_count() == 0 {
            self.content.append(&note("No displays detected."));
            return;
        }

        if !model.records_editable() {
            // R4.4: monitors.conf is unreadable, so the file-backed per-monitor
            // controls are hidden. The runtime laptop toggle is still offered below.
            self.content.append(&note(
                "monitors.conf could not be read, so per-monitor settings are unavailable.",
            ));
        }

        for index in 0..model.monitor_count() {
            self.content.append(&self.build_section(model, index));
        }
    }

    /// Builds one monitor's section: a titled frame with its enable control and, when
    /// enabled and editable, the resolution/refresh/scale/position drop-downs.
    fn build_section(self: &Rc<Self>, model: &DisplayModel, index: usize) -> Frame {
        let title = format!(
            "{} — {}",
            model.monitor_name(index),
            model.monitor_description(index)
        );
        let frame = Frame::new(Some(title.trim_end_matches(" — ")));

        let section = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
        section.set_margin_top(SECTION_SPACING);
        section.set_margin_bottom(SECTION_SPACING);
        section.set_margin_start(SECTION_SPACING);
        section.set_margin_end(SECTION_SPACING);

        // The enable control. A laptop panel's is the runtime toggle (R5.2); a
        // non-laptop's is a staged monitors.conf edit (only when the file is editable).
        if model.is_laptop(index) {
            section.append(&self.build_laptop_toggle(model, index));
        } else if model.records_editable() {
            section.append(&self.build_enable_switch(model, index));
        }

        // The mode/scale/position drop-downs appear only while the output is enabled
        // and the file is editable — there is nothing meaningful to configure on a
        // disabled or unwritable output.
        if model.records_editable() && model.effective_enabled(index) {
            section.append(&self.build_resolution_row(model, index));
            let refresh_options = model.refresh_options(index);
            if !refresh_options.is_empty() {
                section.append(&self.build_refresh_row(model, index, refresh_options));
            }
            section.append(&self.build_scale_row(model, index));
            section.append(&self.build_position_row(model, index));
        }

        frame.set_child(Some(&section));
        frame
    }

    /// The runtime laptop-display enable toggle (R5.2): flipping it applies the panel
    /// on/off live via `hyprctl keyword monitor` and writes/removes the hotplug state
    /// file, never staging or touching `monitors.conf`. Its position reflects the
    /// panel's current live state.
    fn build_laptop_toggle(self: &Rc<Self>, model: &DisplayModel, index: usize) -> GtkBox {
        let switch = Switch::new();
        switch.set_halign(Align::End);
        switch.set_valign(Align::Center);
        switch.set_active(model.laptop_enabled(index));

        let weak = Rc::downgrade(self);
        switch.connect_active_notify(move |switch| {
            if let Some(inner) = weak.upgrade() {
                inner.toggle_laptop(index, switch.is_active());
            }
        });

        labelled_row("Enabled", &switch)
    }

    /// A non-laptop monitor's staged enable switch.
    fn build_enable_switch(self: &Rc<Self>, model: &DisplayModel, index: usize) -> GtkBox {
        let switch = Switch::new();
        switch.set_halign(Align::End);
        switch.set_valign(Align::Center);
        switch.set_active(model.effective_enabled(index));

        let weak = Rc::downgrade(self);
        switch.connect_active_notify(move |switch| {
            if let Some(inner) = weak.upgrade() {
                inner.stage_enabled(index, switch.is_active());
            }
        });

        labelled_row("Enabled", &switch)
    }

    /// The resolution drop-down row.
    fn build_resolution_row(self: &Rc<Self>, model: &DisplayModel, index: usize) -> GtkBox {
        let options = model.resolution_options(index);
        let selected = position_of(&options, &model.effective_resolution(index));
        let weak = Rc::downgrade(self);
        let dropdown = build_dropdown(&options, selected, move |resolution| {
            if let Some(inner) = weak.upgrade() {
                inner.stage_resolution(index, resolution);
            }
        });
        labelled_row("Resolution", &dropdown)
    }

    /// The refresh-rate drop-down row (built only when the resolution has refresh
    /// options).
    fn build_refresh_row(
        self: &Rc<Self>,
        model: &DisplayModel,
        index: usize,
        options: Vec<String>,
    ) -> GtkBox {
        let selected = model
            .effective_refresh(index)
            .and_then(|refresh| position_of(&options, &refresh));
        let weak = Rc::downgrade(self);
        let dropdown = build_dropdown(&options, selected, move |refresh| {
            if let Some(inner) = weak.upgrade() {
                inner.stage_refresh(index, refresh);
            }
        });
        labelled_row("Refresh rate (Hz)", &dropdown)
    }

    /// The scale drop-down row.
    fn build_scale_row(self: &Rc<Self>, model: &DisplayModel, index: usize) -> GtkBox {
        let options = model.scale_options(index);
        let selected = position_of(&options, &model.effective_scale(index));
        let weak = Rc::downgrade(self);
        let dropdown = build_dropdown(&options, selected, move |scale| {
            if let Some(inner) = weak.upgrade() {
                inner.stage_scale(index, scale);
            }
        });
        labelled_row("Scale", &dropdown)
    }

    /// The position drop-down row.
    fn build_position_row(self: &Rc<Self>, model: &DisplayModel, index: usize) -> GtkBox {
        let options = model.position_options(index);
        let selected = position_of(&options, &model.effective_position(index));
        let weak = Rc::downgrade(self);
        let dropdown = build_dropdown(&options, selected, move |position| {
            if let Some(inner) = weak.upgrade() {
                inner.stage_position(index, position);
            }
        });
        labelled_row("Position", &dropdown)
    }

    /// Stages a resolution edit and re-renders.
    fn stage_resolution(self: &Rc<Self>, index: usize, resolution: String) {
        self.stage(|model| model.stage_resolution(index, resolution));
    }

    /// Stages a refresh-rate edit and re-renders.
    fn stage_refresh(self: &Rc<Self>, index: usize, refresh: String) {
        self.stage(|model| model.stage_refresh(index, refresh));
    }

    /// Stages a scale edit and re-renders.
    fn stage_scale(self: &Rc<Self>, index: usize, scale: String) {
        self.stage(|model| model.stage_scale(index, scale));
    }

    /// Stages a position edit and re-renders.
    fn stage_position(self: &Rc<Self>, index: usize, position: String) {
        self.stage(|model| model.stage_position(index, position));
    }

    /// Stages a non-laptop enable edit and re-renders (the dropdowns appear/disappear
    /// with the enabled state).
    fn stage_enabled(self: &Rc<Self>, index: usize, enabled: bool) {
        self.stage(|model| model.stage_enabled(index, enabled));
    }

    /// Applies a staged edit through `edit`, notifies the chrome, then rebuilds.
    ///
    /// The mutable model borrow is released before `on_changed` runs (which re-reads
    /// the model to derive the chrome) and before the rebuild re-reads it.
    fn stage(self: &Rc<Self>, edit: impl FnOnce(&mut DisplayModel)) {
        {
            let mut model = self.model.borrow_mut();
            if let Some(model) = model.as_mut() {
                edit(model);
            }
        }
        (self.on_changed)();
        self.rebuild();
    }

    /// Applies the runtime laptop toggle immediately (R5.2), then rebuilds.
    ///
    /// Runtime-only: it does not stage or change dirty state, so it does not call
    /// `on_changed`; the live `hyprctl keyword monitor` command runs through the real
    /// system runner. The model records the new live-enabled state, which the rebuild
    /// reflects.
    fn toggle_laptop(self: &Rc<Self>, index: usize, enable: bool) {
        let runner = SystemCommandRunner::new();
        {
            let mut model = self.model.borrow_mut();
            if let Some(model) = model.as_mut() {
                if let Err(error) = model.toggle_laptop(index, enable, &runner) {
                    tracing::error!(%error, "the laptop-display toggle could not update its state file");
                }
            }
        }
        self.rebuild();
    }
}

/// Builds the Display page over the shared `model`, reporting staged edits through
/// `on_changed` (task 6.1).
///
/// The returned [`DisplayPage`] must be kept alive by the window: it owns the strong
/// reference to the render state whose handlers keep the model wired. The window
/// mounts [`DisplayPage::root`] in the stack and calls [`DisplayPage::rerender`] after
/// a Reset or a committed Apply.
pub(crate) fn build(
    model: Rc<RefCell<Option<DisplayModel>>>,
    on_changed: Rc<dyn Fn()>,
) -> DisplayPage {
    let content = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    content.set_margin_top(PAGE_MARGIN);
    content.set_margin_bottom(PAGE_MARGIN);
    content.set_margin_start(PAGE_MARGIN);
    content.set_margin_end(PAGE_MARGIN);

    let inner = Rc::new(Inner {
        content: content.clone(),
        model,
        on_changed,
    });
    inner.rebuild();

    let root = ScrolledWindow::new();
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.set_child(Some(&content));

    DisplayPage { root, inner }
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

/// The index of `value` in `options`, for preselecting a drop-down.
fn position_of(options: &[String], value: &str) -> Option<u32> {
    options
        .iter()
        .position(|option| option == value)
        .map(|index| index as u32)
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
