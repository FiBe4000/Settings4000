//! The Sound page's bespoke GTK glue (task 6.2; architecture §7; R3.1, R5.2).
//!
//! # Why this page is bespoke, not declarative
//!
//! Every file-backed §6 page is a list of fixed
//! [`RowDescriptor`](super::row::RowDescriptor)s rendered by the declarative framework
//! ([`super::page`]), which stages edits into the [`SettingsStore`](crate::core::store)
//! and drives the Apply/Reset chrome. The Sound page fits neither: it is **entirely
//! runtime-only** (R3.1/R5.2). PipeWire keeps no dotfile here, so every control applies
//! *immediately* by running a `wpctl` command — nothing is staged, nothing is dirty,
//! there is no Apply/Reset involvement, and it never touches the store. The declarative
//! framework renders from `store.value(id)` and `debug_assert!`s file-backed settings,
//! so a runtime control has no place there. This page therefore renders directly from
//! the GTK-free [`SoundState`](crate::core::sound::SoundState), mirroring the bespoke
//! Display page ([`super::display`]).
//!
//! # Render-from-state, apply-immediately
//!
//! On page entry the page enumerates the live audio devices
//! ([`sound::enumerate`](crate::core::sound::enumerate) — `pw-dump` JSON, falling back
//! to `wpctl status`) and renders one section per output/input: a device drop-down, a
//! volume slider, and a mute switch, reflecting the *default* device of that kind.
//! Driving any control runs the matching `wpctl` command at once
//! ([`sound::set_default`](crate::core::sound::set_default) /
//! [`set_volume`](crate::core::sound::set_volume) /
//! [`set_mute`](crate::core::sound::set_mute)) through the real system runner.
//!
//! Switching the default device re-enumerates and rebuilds, so the volume/mute controls
//! then target the newly-default device; volume/mute changes do not rebuild (a slider
//! drag must not tear down the widget it is driving). A "Rescan devices" button and the
//! window re-showing the page both re-enumerate, so external volume changes are picked
//! up. Each control sets its widget value **before** connecting the change handler, so a
//! programmatic render never masquerades as a user edit (the same discipline the Display
//! page follows).
//!
//! # Synchronous on the GTK main thread (deliberate)
//!
//! The `wpctl`/`pw-dump` calls are short-lived, so — matching the Display page's
//! convention (task 6.1) — they run synchronously on the GTK main thread through
//! [`CommandRunner::run`](crate::system::command::CommandRunner::run) rather than being
//! pushed to a worker. This keeps the runtime controls simple and immediate; there is no
//! staging pipeline to coordinate with, so no async machinery is warranted.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, DropDown, Frame, Label, Orientation, Scale, ScrolledWindow,
    StringList, Switch, Widget, glib,
};

use crate::core::sound::{self, SoundDevice, SoundState};
use crate::system::command::SystemCommandRunner;

/// Outer margin, in pixels, around the page content.
const PAGE_MARGIN: i32 = 18;

/// Vertical spacing, in pixels, between sections and between rows.
const SECTION_SPACING: i32 = 12;

/// Horizontal spacing, in pixels, between a row label and its control.
const ROW_SPACING: i32 = 8;

/// The volume slider range: a 0–100 percentage over the `wpctl`-scale `0.0`..=`1.0`.
const VOLUME_PERCENT_MAX: f64 = 100.0;

/// How long after the last volume-slider movement the `wpctl set-volume` command is
/// actually run.
///
/// A slider drag emits a `value-changed` for every 1% step, so applying on each would
/// spawn ~100 short-lived `wpctl` processes back-to-back on the UI thread. Instead each
/// change (re)arms a single one-shot timer and only the *last* value is applied once the
/// user pauses or releases for this long — coalescing a whole drag into one command,
/// while staying on the main thread (per the accepted synchronous-`wpctl` convention).
const VOLUME_DEBOUNCE: Duration = Duration::from_millis(150);

/// The mounted Sound page: the scrollable root plus the handle the window uses to
/// re-enumerate it when the page is re-shown.
pub(crate) struct SoundPage {
    /// The scrollable widget mounted in the window's stack.
    root: ScrolledWindow,
    /// The shared render state; kept alive for the life of the page (its controls'
    /// handlers hold only [`std::rc::Weak`] references, so this strong reference is what
    /// keeps the state — and thus the handlers — alive).
    inner: Rc<Inner>,
}

impl SoundPage {
    /// The widget to add to the window's stack.
    pub(crate) fn root(&self) -> &ScrolledWindow {
        &self.root
    }

    /// Re-enumerates the audio devices and rebuilds the controls — called by the window
    /// when the Sound page is (re-)shown, so external volume/device changes are picked up
    /// on page entry (R3.1).
    pub(crate) fn refresh(&self) {
        self.inner.reenumerate();
    }
}

/// The shared render state the page's control handlers operate on.
///
/// Handlers capture a [`std::rc::Weak`] to this and upgrade on use, so the widget tree
/// (owned via `content`) never forms a reference cycle with the closures mounted inside
/// it.
struct Inner {
    /// The vertical box holding the rescan button and one section per device kind,
    /// rebuilt in place.
    content: GtkBox,
    /// The last-enumerated audio state, the single source of truth the controls render
    /// from. Refreshed by [`Self::reenumerate`]; it is not the store — the Sound page
    /// stages nothing (R5.2).
    state: RefCell<SoundState>,
    /// The pending, debounced volume-apply timer, if a slider was moved within the last
    /// [`VOLUME_DEBOUNCE`]. Held so a subsequent movement can cancel and re-arm it,
    /// coalescing a drag into a single `wpctl set-volume`.
    volume_timeout: RefCell<Option<glib::SourceId>>,
}

impl Inner {
    /// Re-enumerates the live audio devices and rebuilds the controls (R3.1).
    fn reenumerate(self: &Rc<Self>) {
        // Drop any pending debounced volume apply: the sliders are about to be rebuilt
        // from fresh state, so a stale in-flight apply would fight the new values.
        self.cancel_volume_timeout();
        let state = sound::enumerate(&SystemCommandRunner::new());
        *self.state.borrow_mut() = state;
        self.rebuild();
    }

    /// Rebuilds the page: a rescan button and the output/input sections (R3.1).
    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        self.content.append(&self.build_rescan_button());

        let state = self.state.borrow();
        self.content
            .append(&self.build_device_section("Output", state.outputs()));
        self.content
            .append(&self.build_device_section("Input", state.inputs()));
    }

    /// The "Rescan devices" button that re-enumerates on demand.
    fn build_rescan_button(self: &Rc<Self>) -> Button {
        let button = Button::with_label("Rescan devices");
        button.set_halign(Align::Start);
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.reenumerate();
            }
        });
        button
    }

    /// Builds one device kind's section: a device drop-down (switch the default), and —
    /// for the selected (default) device — a volume slider and a mute switch (R3.1).
    ///
    /// An empty device list renders a plain note instead of controls.
    fn build_device_section(self: &Rc<Self>, title: &str, devices: &[SoundDevice]) -> Frame {
        let frame = Frame::new(Some(title));
        let section = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
        section.set_margin_top(SECTION_SPACING);
        section.set_margin_bottom(SECTION_SPACING);
        section.set_margin_start(SECTION_SPACING);
        section.set_margin_end(SECTION_SPACING);

        if devices.is_empty() {
            section.append(&note(&format!(
                "No {} devices found.",
                title.to_ascii_lowercase()
            )));
            frame.set_child(Some(&section));
            return frame;
        }

        // The device drop-down: choosing an entry makes it the default of its kind via
        // `wpctl set-default`. The default device is preselected.
        let ids: Vec<u32> = devices.iter().map(SoundDevice::id).collect();
        let labels: Vec<String> = devices.iter().map(|d| d.label().to_string()).collect();
        let selected = devices
            .iter()
            .position(SoundDevice::is_default)
            .unwrap_or(0);

        let weak = Rc::downgrade(self);
        let dropdown = build_dropdown(&labels, selected as u32, move |index| {
            if let Some(inner) = weak.upgrade() {
                if let Some(&id) = ids.get(index) {
                    inner.set_default(id);
                }
            }
        });
        section.append(&labelled_row("Device", &dropdown));

        // The volume and mute controls act on the selected (default) device.
        let device = &devices[selected];
        section.append(&self.build_volume_row(device.id(), device.volume()));
        section.append(&self.build_mute_row(device.id(), device.muted()));

        frame.set_child(Some(&section));
        frame
    }

    /// The volume slider row for device `id`, initialised to its `wpctl`-scale `volume`
    /// as a 0–100 percentage. Dragging it runs `wpctl set-volume` immediately.
    fn build_volume_row(self: &Rc<Self>, id: u32, volume: f64) -> GtkBox {
        let scale = Scale::with_range(Orientation::Horizontal, 0.0, VOLUME_PERCENT_MAX, 1.0);
        scale.set_hexpand(true);
        scale.set_draw_value(true);
        scale.set_round_digits(0);
        // Set the value before connecting the handler, so this programmatic set is not
        // mistaken for a user drag.
        scale.set_value((volume * VOLUME_PERCENT_MAX).round());

        let weak = Rc::downgrade(self);
        scale.connect_value_changed(move |scale| {
            if let Some(inner) = weak.upgrade() {
                // Debounced: a drag re-arms one timer rather than spawning `wpctl` per
                // 1% step. See `schedule_volume`.
                inner.schedule_volume(id, scale.value() / VOLUME_PERCENT_MAX);
            }
        });
        labelled_row("Volume", &scale)
    }

    /// The mute switch row for device `id`, initialised to its mute state. Toggling it
    /// runs `wpctl set-mute` immediately.
    fn build_mute_row(self: &Rc<Self>, id: u32, muted: bool) -> GtkBox {
        let switch = Switch::new();
        switch.set_halign(Align::End);
        switch.set_valign(Align::Center);
        switch.set_active(muted);

        let weak = Rc::downgrade(self);
        switch.connect_active_notify(move |switch| {
            if let Some(inner) = weak.upgrade() {
                inner.set_mute(id, switch.is_active());
            }
        });
        labelled_row("Muted", &switch)
    }

    /// Switches the default device to `id` immediately (R5.2), then re-enumerates and
    /// rebuilds so the volume/mute rows target the newly-default device.
    ///
    /// This is called from the device drop-down's `selected` handler, so the
    /// `reenumerate` → `rebuild` here removes the very `DropDown` that is mid-emission.
    /// That re-entrant teardown is **intentional and safe**: GTK4 keeps the emitting
    /// widget alive for the duration of the signal emission (the handler holds only a
    /// `Weak`, and the widget is dropped only after the emission unwinds), and it is
    /// validated live. Do not "fix" it into something that looks less like a
    /// use-after-free — the Display page (task 6.1) relies on the same pattern.
    fn set_default(self: &Rc<Self>, id: u32) {
        sound::set_default(&SystemCommandRunner::new(), id);
        self.reenumerate();
    }

    /// (Re)arms the debounced volume-apply timer for device `id` at the `wpctl`-scale
    /// `volume`, cancelling any pending one so only the latest value is applied.
    ///
    /// The one-shot timer fires [`VOLUME_DEBOUNCE`] after the last movement, so a drag —
    /// which emits a change per 1% step — collapses into a single `wpctl set-volume`
    /// rather than a flood of them. The closure holds a [`std::rc::Weak`] to the page,
    /// so if the page is torn down before it fires the apply is simply skipped.
    fn schedule_volume(self: &Rc<Self>, id: u32, volume: f64) {
        self.cancel_volume_timeout();
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(VOLUME_DEBOUNCE, move || {
            if let Some(inner) = weak.upgrade() {
                // The timer has fired and removed itself; drop the stale handle before
                // running the command so a later cancel does not try to remove it.
                inner.volume_timeout.borrow_mut().take();
                sound::set_volume(&SystemCommandRunner::new(), id, volume);
            }
        });
        *self.volume_timeout.borrow_mut() = Some(source);
    }

    /// Cancels a pending debounced volume apply, if any.
    fn cancel_volume_timeout(&self) {
        if let Some(source) = self.volume_timeout.borrow_mut().take() {
            source.remove();
        }
    }

    /// Mutes/unmutes device `id` immediately (R5.2). No rebuild — the switch already
    /// shows the new state.
    fn set_mute(&self, id: u32, muted: bool) {
        sound::set_mute(&SystemCommandRunner::new(), id, muted);
    }
}

/// Builds the Sound page, enumerating the audio devices on entry (task 6.2).
///
/// The returned [`SoundPage`] must be kept alive by the window: it owns the strong
/// reference to the render state whose handlers keep the controls wired. The window
/// mounts [`SoundPage::root`] in the stack and calls [`SoundPage::refresh`] when the page
/// is re-shown.
pub(crate) fn build() -> SoundPage {
    let content = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    content.set_margin_top(PAGE_MARGIN);
    content.set_margin_bottom(PAGE_MARGIN);
    content.set_margin_start(PAGE_MARGIN);
    content.set_margin_end(PAGE_MARGIN);

    let inner = Rc::new(Inner {
        content: content.clone(),
        state: RefCell::new(SoundState::default()),
        volume_timeout: RefCell::new(None),
    });
    // Enumerate + build now (page entry, R3.1).
    inner.reenumerate();

    let root = ScrolledWindow::new();
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.set_child(Some(&content));

    SoundPage { root, inner }
}

/// Builds a `GtkDropDown` over `labels`, preselecting index `selected` and invoking
/// `on_selected` with the chosen index on a user change.
///
/// The selection is set **before** the change handler is connected, so the programmatic
/// set never fires the handler.
fn build_dropdown(
    labels: &[String],
    selected: u32,
    on_selected: impl Fn(usize) + 'static,
) -> DropDown {
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let model = StringList::new(&refs);
    let dropdown = DropDown::builder().model(&model).build();
    dropdown.set_halign(Align::End);
    dropdown.set_valign(Align::Center);
    dropdown.set_selected(selected);

    dropdown.connect_selected_notify(move |dropdown| {
        on_selected(dropdown.selected() as usize);
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
