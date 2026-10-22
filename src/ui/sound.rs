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
//! # The first enumeration is deferred to first page entry (deliberate)
//!
//! [`build`] runs no `pw-dump`/`wpctl` at all (task 9.4) — the page starts with a
//! placeholder and the first enumeration happens when it first becomes the visible stack
//! child, through the same window hook that re-enumerates on every later entry, which
//! runs it from the GTK idle queue. The Network page ([`super::network`]) defers its first
//! read the same way, for the same reason: the page is built inside the window's populate
//! pass on the GTK main thread, and a wedged PipeWire/WirePlumber would hold a synchronous
//! `pw-dump` for the full 5 s command timeout — stalling *every* category's appearance
//! against the R8.1 cold-start budget, for a page the user may never open. The idle hop is
//! what makes that true in *all* configurations: adding the first page to the stack makes
//! it visible and fires the entry hook synchronously, so on a host where Sound is the
//! first visible category (no `hyprctl`, so no Display page) an immediate probe would run
//! inside populate anyway. The mechanism lives in `wire_deferred_page_entry`
//! ([`super::window`]), whose docs spell out what the idle callback re-checks.
//!
//! Deferring also keeps a manual detection refresh (R4.3), which rebuilds all pages, from
//! re-enumerating a page nobody is looking at. It costs nothing: the placeholder is
//! visible at most for the instant before the idle callback runs. Nothing is lost by
//! rebuilding into it either — the page is entirely runtime-only, so it holds no staged
//! edit a repopulate could discard (R5.2).
//!
//! `tests/page_entry_deferral.rs` guards this at the source level for this page and
//! its runtime-backed siblings: nothing reachable from [`build`] may spawn a command,
//! and the page-entry `refresh` must reach the live read.
//!
//! # Synchronous on the GTK main thread (deliberate)
//!
//! Once the page is actually viewed, the `wpctl`/`pw-dump` calls are short-lived, so —
//! matching the Display page's convention (task 6.1) — they run synchronously on the GTK
//! main thread through
//! [`CommandRunner::run`](crate::system::command::CommandRunner::run) rather than being
//! pushed to a worker. This keeps the runtime controls simple and immediate; there is no
//! staging pipeline to coordinate with, so no async machinery is warranted.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Frame, Orientation, Scale, ScrolledWindow, Switch, glib};

use crate::core::sound::{self, SoundDevice, SoundState};
use crate::system::command::SystemCommandRunner;
use crate::ui::widgets::{
    PAGE_MARGIN, SECTION_SPACING, build_indexed_dropdown, labelled_row, note,
};

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
/// enumerate it whenever the page is shown.
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

    /// Enumerates the audio devices and rebuilds the controls — called by the window
    /// whenever the Sound page becomes the visible stack child, which covers both the
    /// deferred *first* enumeration (see the module docs) and picking up a volume/device
    /// change made elsewhere while the app sat on another page (R3.1).
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
    ///
    /// It starts out empty and is never *rendered* in that state: [`build`] shows a
    /// placeholder instead and the first enumeration happens on first page entry (see the
    /// module docs), so [`Self::rebuild`] only ever runs on enumerated state.
    state: RefCell<SoundState>,
    /// The pending, debounced volume-apply timer, if a slider was moved within the last
    /// [`VOLUME_DEBOUNCE`]. Held so a subsequent movement can cancel and re-arm it,
    /// coalescing a drag into a single `wpctl set-volume`.
    volume_timeout: RefCell<Option<glib::SourceId>>,
}

impl Inner {
    /// Enumerates the live audio devices and rebuilds the controls (R3.1).
    ///
    /// The only place the page probes PipeWire, and thus the only place it spawns a
    /// subprocess for reading state: page entry, the "Rescan devices" button, and a
    /// default-device switch all come through here. [`build`] deliberately does not (see
    /// the module docs).
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
        let dropdown = build_indexed_dropdown(&labels, Some(selected as u32), move |index| {
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

/// Builds the Sound page (task 6.2).
///
/// Deliberately runs **no** `pw-dump`/`wpctl` here — the page starts with a placeholder
/// and the first enumeration happens when it first becomes the visible stack child (see
/// the module docs for the R8.1 startup-budget rationale). The returned [`SoundPage`] must
/// be kept alive by the window: it owns the strong reference to the render state whose
/// handlers keep the controls wired. The window mounts [`SoundPage::root`] in the stack
/// and calls [`SoundPage::refresh`] whenever the page is shown.
pub(crate) fn build() -> SoundPage {
    let content = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    content.set_margin_top(PAGE_MARGIN);
    content.set_margin_bottom(PAGE_MARGIN);
    content.set_margin_start(PAGE_MARGIN);
    content.set_margin_end(PAGE_MARGIN);

    // The placeholder content, replaced wholesale by the first enumeration, which the
    // window's page-entry hook triggers the moment the page becomes visible.
    content.append(&note("Reading the audio devices…"));

    let inner = Rc::new(Inner {
        content: content.clone(),
        state: RefCell::new(SoundState::default()),
        volume_timeout: RefCell::new(None),
    });

    let root = ScrolledWindow::new();
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.set_child(Some(&content));

    SoundPage { root, inner }
}
