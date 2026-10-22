//! The Display page's bespoke GTK glue (tasks 6.1, 11.8, 11.9; architecture §7;
//! R2.3, R4.2, R4.4, R5.2, R8.3).
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
//! # Refused edits (R8.3; task 11.9)
//!
//! The model refuses an edit whose value is malformed or whose resulting
//! `monitors.conf` would break the file's frozen grammar, returning a
//! [`StageRefusal`](crate::core::display::StageRefusal) that names the rule. The
//! rebuild then shows the control at its unchanged value with a "Not staged: …"
//! note under that monitor, the same note-under-the-control pattern the laptop
//! toggle uses for a switch that did not take (R5.5). The note stays until the
//! next edit, Reset or Apply. A `monitors.conf` that already breaks the grammar
//! makes the whole page read-only: the per-monitor controls are hidden, as for
//! an unreadable file (R4.4), with a note naming the offending line and rule.
//!
//! # Staged vs. runtime controls (R5.1/R5.2)
//!
//! A non-laptop monitor's resolution/refresh/scale/position edits, and switching an
//! output that is off back on, are **staged** into the model and written to
//! `monitors.conf` on Apply (there is no switch to turn a running external output off:
//! `monitors.conf`'s frozen grammar has no record form for it, task 11.9); the page
//! reports each
//! through the shared `on_changed` callback so the window's Apply/Reset chrome lights
//! up (as the declarative pages do). The laptop panel's enable control is
//! **runtime-only**: toggling it calls [`DisplayModel::toggle_laptop`] immediately,
//! which runs the dotfiles' `scripts/hypr-toggle-laptop-display` between two reads of
//! the compositor's monitor list (task 11.8), never staging and never marking the
//! page dirty. The switch is re-drawn from the state *read* afterwards, so a toggle
//! that did not take snaps back and shows why underneath. The control exists only
//! when detection found the script (R4.2). Its mode/scale, by contrast, are staged
//! like any monitor's (the single-source gotcha, analysis §6.2).
//!
//! The script runs synchronously on the GTK main thread through the real
//! [`CommandRunner`](crate::system::command::CommandRunner), like the other runtime
//! controls (Sound, Network, Notifications). Its 5 s timeout bounds the script
//! itself, **not** the wait for its bar-relaunch child: on a timeout the runner
//! kills only the script's shell and then keeps reading the captured output until
//! the orphaned `launch-eww-bars` closes the pipes it inherited, so a slow eww
//! restart can freeze the window for longer. Moving the toggle off the main thread
//! is a recorded follow-up in `docs/tasks.md`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Frame, Orientation, ScrolledWindow, Switch};

use crate::core::display::{DisplayModel, StageRefusal};
use crate::system::command::SystemCommandRunner;
use crate::ui::widgets::{PAGE_MARGIN, SECTION_SPACING, build_dropdown, labelled_row, note};

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
        // A Reset or Apply is unrelated to the last laptop toggle or refused edit,
        // so their notes must not survive into the re-rendered page.
        self.inner.laptop_error.replace(None);
        self.inner.stage_error.replace(None);
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
    /// Why the last laptop toggle did not take, shown under the switch until the
    /// next toggle (R5.5), or `None` after a successful one.
    laptop_error: RefCell<Option<String>>,
    /// The monitor index and reason of the last refused staged edit (R8.3), shown
    /// under that monitor's controls until the next edit, or `None` after an
    /// accepted one.
    stage_error: RefCell<Option<(usize, String)>>,
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

        if let Some(problem) = model.records_problem() {
            // Task 11.9: monitors.conf breaks the frozen grammar its two parsers rely
            // on, so the page is read-only — the app never edits around a record it
            // would refuse to write. The runtime laptop toggle is still offered below.
            self.content.append(&note(&format!(
                "monitors.conf breaks the format that hyprland.lua and \
                 hypr-display-profile.sh rely on ({problem}), so per-monitor settings are \
                 unavailable until that line is fixed."
            )));
        } else if !model.records_editable() {
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

        // The enable control. A laptop panel's is the runtime toggle (R5.2), shown
        // only when the dotfiles script behind it was detected (R4.2). A non-laptop's
        // is a staged monitors.conf edit (only when the file is editable), offered
        // only while the output is off: the frozen grammar has no record form that
        // turns an output off (task 11.9), so a switch on a running output could only
        // ever refuse. The model refuses a disable too, as the defence behind this.
        if model.is_laptop(index) {
            if model.laptop_toggle_available(index) {
                section.append(&self.build_laptop_toggle(model, index));
                if let Some(error) = self.laptop_error.borrow().as_deref() {
                    section.append(&note(error));
                }
            }
        } else if model.records_editable() {
            if model.effective_enabled(index) {
                section.append(&note(
                    "Turning an external monitor off is not supported yet.",
                ));
            } else {
                section.append(&self.build_enable_switch(model, index));
            }
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

        // Why the last edit on this monitor was refused (R8.3).
        if let Some((_, reason)) = self
            .stage_error
            .borrow()
            .as_ref()
            .filter(|(refused, _)| *refused == index)
        {
            section.append(&note(reason));
        }

        frame.set_child(Some(&section));
        frame
    }

    /// The runtime laptop-display enable toggle (R5.2): flipping it runs the dotfiles'
    /// toggle script (task 11.8), never staging or touching `monitors.conf`. Its
    /// position is the panel's last *read* state.
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

    /// A non-laptop monitor's staged enable switch, built only while the output is
    /// off, so turning it on stages a complete record for the output (see
    /// [`DisplayModel::stage_enabled`]). Once it is staged on, the rebuild shows the
    /// "not supported yet" note and the output's mode/scale rows in its place; Reset
    /// discards the staged enable.
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
        self.stage(index, |model| model.stage_resolution(index, resolution));
    }

    /// Stages a refresh-rate edit and re-renders.
    fn stage_refresh(self: &Rc<Self>, index: usize, refresh: String) {
        self.stage(index, |model| model.stage_refresh(index, refresh));
    }

    /// Stages a scale edit and re-renders.
    fn stage_scale(self: &Rc<Self>, index: usize, scale: String) {
        self.stage(index, |model| model.stage_scale(index, scale));
    }

    /// Stages a position edit and re-renders.
    fn stage_position(self: &Rc<Self>, index: usize, position: String) {
        self.stage(index, |model| model.stage_position(index, position));
    }

    /// Stages a non-laptop enable edit and re-renders (the dropdowns appear/disappear
    /// with the enabled state).
    fn stage_enabled(self: &Rc<Self>, index: usize, enabled: bool) {
        self.stage(index, |model| model.stage_enabled(index, enabled));
    }

    /// Applies a staged edit to monitor `index` through `edit`, records why it was
    /// refused (if it was), notifies the chrome, then rebuilds.
    ///
    /// The model has already logged a refusal; here it is only kept for the rebuild
    /// to show under the monitor, whose control re-renders at its unchanged value.
    /// The mutable model borrow is released before `on_changed` runs (which re-reads
    /// the model to derive the chrome) and before the rebuild re-reads it.
    fn stage(
        self: &Rc<Self>,
        index: usize,
        edit: impl FnOnce(&mut DisplayModel) -> Result<(), StageRefusal>,
    ) {
        let result = self.model.borrow_mut().as_mut().map(edit);
        self.stage_error.replace(match result {
            Some(Err(refusal)) => Some((index, format!("Not staged: {refusal}."))),
            Some(Ok(())) | None => None,
        });
        (self.on_changed)();
        self.rebuild();
    }

    /// Applies the runtime laptop toggle immediately (R5.2), then rebuilds.
    ///
    /// Runtime-only: it does not stage or change dirty state, so it does not call
    /// `on_changed`. The script and the reads around it run through the real system
    /// runner; the model has already logged any failure, so here it is only kept for
    /// the rebuild to show under the switch, which is re-drawn from the read state.
    fn toggle_laptop(self: &Rc<Self>, index: usize, enable: bool) {
        let runner = SystemCommandRunner::new();
        let result = self
            .model
            .borrow_mut()
            .as_mut()
            .map(|model| model.toggle_laptop(index, enable, &runner));
        self.laptop_error.replace(match result {
            Some(Err(error)) => Some(format!("The laptop display was not switched: {error}.")),
            Some(Ok(())) | None => None,
        });
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
        laptop_error: RefCell::new(None),
        stage_error: RefCell::new(None),
    });
    inner.rebuild();

    let root = ScrolledWindow::new();
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.set_child(Some(&content));

    DisplayPage { root, inner }
}

/// The index of `value` in `options`, for preselecting a drop-down.
fn position_of(options: &[String], value: &str) -> Option<u32> {
    options
        .iter()
        .position(|option| option == value)
        .map(|index| index as u32)
}
