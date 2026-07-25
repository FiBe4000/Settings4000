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

#[cfg(test)]
mod tests {
    //! Guards for the deferral contract of task 9.4: nothing this module runs while the
    //! window *builds* the page may spawn a subprocess, and the page-entry refresh must be
    //! what does the probing.
    //!
    //! These are **source-level** guards, not behavioural ones, because the behaviour
    //! cannot be reached headlessly: constructing any widget requires an initialised GTK,
    //! which needs a display server, and the test suite runs without one (CI installs only
    //! the GTK development libraries). So instead of calling [`build`] with a mock command
    //! runner — which is how the enumeration itself is tested, in [`crate::core::sound`]'s
    //! unit tests — these tests parse this module's own source, walk the call graph
    //! outwards from a chosen entry point, and look for anything that can spawn a command.
    //! They catch the exact regression this task fixed (an enumeration on the populate
    //! path, R8.1) including its indirect form, a small helper that probes on `build`'s
    //! behalf.
    //!
    //! `tests/module_boundaries.rs` and `tests/no_custom_css.rs` guard other
    //! compiler-inexpressible rules the same way, and like them these guards blank comments
    //! first so the rule can be *documented* in this file without tripping it. The stripper
    //! here is deliberately smaller than theirs: it removes `//` comments only and asserts
    //! that this module contains no `/* */` block comment (rather than lexing one), and it
    //! does not parse string literals, so a `//` inside a string would truncate the rest of
    //! that line. Neither shortcut is load-bearing today and both fail loudly — the block
    //! comment assertion by name, a truncation only by weakening one line of one function —
    //! but a future maintainer adding either construct should extend the stripper.
    //!
    //! What the guards cannot see: [`Shell::populate`](crate::ui::window) itself, which is
    //! GTK-bound and therefore not headlessly testable at all. That the *whole* populate
    //! path stays subprocess-free was verified against the running desktop instead
    //! (recorded in `docs/tasks.md`, task 9.4).

    /// This module's own source, embedded at compile time, so the guards need neither a
    /// filesystem lookup nor a GTK runtime.
    const SOURCE: &str = include_str!("sound.rs");

    /// Everything that can spawn a subprocess from this module: the real command runner it
    /// hands to [`crate::core::sound`], the trait behind it, the enumeration entry points
    /// that take one, and — should anyone bypass the [`CommandRunner`] seam entirely, which
    /// the architecture forbids — `std::process::Command`. Matched as substrings, so
    /// `reenumerate` counts as reaching `enumerate`, which is exactly right: reaching it
    /// *is* reaching a probe. Importing the runner under an alias would hide that one
    /// marker (the call site would read `Runner::new()`), which is why the list also names
    /// the enumeration functions and the raw process API — a probe has to trip at least one
    /// of them.
    const PROBE_MARKERS: &[&str] = &[
        "SystemCommandRunner",
        "CommandRunner",
        "enumerate",
        "process::Command",
        "Command::new",
    ];

    /// The markers that must genuinely occur in this module's code, checked by
    /// [`probe_markers_are_not_stale`].
    ///
    /// Without this, renaming (say) `SystemCommandRunner` would silently disable half the
    /// rule: the guards would keep passing while no longer guarding anything. The other
    /// entries in [`PROBE_MARKERS`] are defence in depth against code that does not exist
    /// here today, so they are deliberately not required to appear.
    const LIVE_MARKERS: &[&str] = &["SystemCommandRunner", "enumerate"];

    /// This module's production source with comments blanked — everything above the test
    /// module, so the guards never see their own marker strings.
    fn production_code() -> String {
        let end = SOURCE
            .find("#[cfg(test)]")
            .expect("this module's own test attribute must be findable in its source");
        strip_comments(&SOURCE[..end])
    }

    /// `code` with every `//` comment removed, so the rules can be documented in rustdoc
    /// without tripping the guards (see the module docs for this stripper's limits).
    fn strip_comments(code: &str) -> String {
        assert!(
            !code.contains("/*"),
            "this guard's comment stripper only understands `//` comments; a `/* */` block \
             comment was added to src/ui/sound.rs, so the stripper needs extending"
        );
        code.lines()
            .map(|line| match line.find("//") {
                Some(index) => &line[..index],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every function defined in `code`, as `(name, body)` pairs, where the body is the
    /// source text between the signature's opening brace and its closing one.
    ///
    /// Bounded by indentation rather than by brace counting: `rustfmt` — a CI gate here —
    /// puts an item's closing brace at exactly the indentation of the line its signature
    /// starts on and indents everything nested inside deeper, so the first following line
    /// consisting of that indentation plus `}` ends the body. Only definitions are
    /// collected: an `fn` preceded on its line by anything other than a visibility keyword
    /// (a call, an `impl Fn(..)` bound, a type) is skipped.
    fn functions(code: &str) -> Vec<(&str, &str)> {
        let mut functions = Vec::new();
        let mut search = 0;

        while let Some(relative) = code[search..].find("fn ") {
            let keyword = search + relative;
            search = keyword + "fn ".len();

            let line_start = code[..keyword].rfind('\n').map_or(0, |index| index + 1);
            let prefix = &code[line_start..keyword];
            if !matches!(prefix.trim(), "" | "pub" | "pub(crate)" | "pub(super)") {
                continue;
            }
            let indent: String = prefix
                .chars()
                .take_while(char::is_ascii_whitespace)
                .collect();

            let name_start = search;
            let name_end = name_start
                + code[name_start..]
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .expect("a function name is always followed by `(` or `<`");
            let Some(body_start) = code[name_end..].find('{').map(|index| name_end + index + 1)
            else {
                continue;
            };
            let terminator = format!("\n{indent}}}");
            let Some(body_end) = code[body_start..]
                .find(&terminator)
                .map(|index| body_start + index)
            else {
                continue;
            };

            functions.push((&code[name_start..name_end], &code[body_start..body_end]));
        }

        functions
    }

    /// Whether `body` mentions `name` as a whole word, i.e. as a call or a reference rather
    /// than as part of a longer identifier (so `build` does not match `build_dropdown`).
    fn mentions(body: &str, name: &str) -> bool {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        body.match_indices(name).any(|(index, _)| {
            let before = body[..index].chars().next_back();
            let after = body[index + name.len()..].chars().next();
            !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
        })
    }

    /// The names of the functions `entry` can run synchronously, transitively, including
    /// `entry` itself.
    ///
    /// Closure bodies count as part of the function they are written in. That is
    /// conservative — a widget handler that probes when the *user* acts would be reported as
    /// if `build` probed — and it is why `build` installs no handlers: keeping every
    /// probe-capable closure out of it is what makes this guard exact. A future page
    /// skeleton that needs a handler in `build` has to rework the guard rather than
    /// quietly loosen it.
    fn reachable_from<'a>(entry: &str, functions: &[(&'a str, &'a str)]) -> Vec<&'a str> {
        let body_of = |name: &str| {
            functions
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, body)| *body)
        };
        assert!(
            body_of(entry).is_some(),
            "the guard could not find `{entry}` in src/ui/sound.rs — the function was \
             renamed or the extraction above broke, either of which would make these \
             guards vacuous"
        );

        let mut reached: Vec<&'a str> = Vec::new();
        let mut pending = vec![
            functions
                .iter()
                .find(|(name, _)| *name == entry)
                .map(|(name, _)| *name)
                .expect("checked just above"),
        ];

        while let Some(name) = pending.pop() {
            if reached.contains(&name) {
                continue;
            }
            reached.push(name);
            let Some(body) = body_of(name) else { continue };
            for (candidate, _) in functions {
                if !reached.contains(candidate) && mentions(body, candidate) {
                    pending.push(candidate);
                }
            }
        }

        reached
    }

    /// The first `(function, marker)` pair showing that `entry` can spawn a subprocess, or
    /// `None` when nothing reachable from it can.
    fn probe_reachable_from(entry: &str) -> Option<(String, &'static str)> {
        let code = production_code();
        let functions = functions(&code);
        reachable_from(entry, &functions).iter().find_map(|name| {
            let body = functions
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, body)| *body)
                .unwrap_or_default();
            PROBE_MARKERS
                .iter()
                .find(|marker| body.contains(*marker))
                .map(|marker| ((*name).to_string(), *marker))
        })
    }

    #[test]
    fn build_runs_no_command_so_populate_cannot_stall() {
        // The accept criterion of task 9.4: nothing is spawned while the window's
        // `populate` builds the page — not by `build` itself and not by a helper it calls.
        // A synchronous `pw-dump` against a wedged PipeWire would block the main thread for
        // the full 5 s command timeout and delay every category's appearance (R8.1).
        if let Some((function, marker)) = probe_reachable_from("build") {
            panic!(
                "sound::build can spawn a subprocess: `{function}` reaches `{marker}`. The \
                 first enumeration is deferred to first page entry precisely so nothing \
                 runs on the populate path (task 9.4, R8.1)"
            );
        }
    }

    #[test]
    fn the_page_entry_refresh_is_what_enumerates() {
        // The other half of the contract: with `build` no longer enumerating, the window's
        // page-entry hook (`Shell::wire_sound_page_entry` → this `refresh`) is the only
        // thing that fills the page, so it must still reach the enumeration — otherwise the
        // deferral would turn into a page that never shows a device.
        assert!(
            probe_reachable_from("refresh").is_some(),
            "SoundPage::refresh must reach the device enumeration: it performs both the \
             deferred first enumeration and every later page entry (task 9.4, R3.1)"
        );

        let code = production_code();
        let functions = functions(&code);
        let reachable = reachable_from("refresh", &functions);
        assert!(
            reachable.contains(&"reenumerate"),
            "the enumeration is expected to be reached through `Inner::reenumerate`; \
             reachable from `refresh`: {reachable:?}"
        );
    }

    #[test]
    fn the_reachability_walk_follows_calls() {
        // Without this, a walk that only ever returned its entry point would make the
        // guard above pass no matter what a helper called from `build` does. `note` is the
        // placeholder label helper `build` calls, so it must show up.
        let code = production_code();
        let reachable = reachable_from("build", &functions(&code));
        assert!(
            reachable.contains(&"note"),
            "expected the walk from `build` to reach its `note` helper; reached: \
             {reachable:?}"
        );
    }

    #[test]
    fn probe_markers_are_not_stale() {
        // A rename must break this test loudly rather than silently disable the rule.
        let code = production_code();
        for marker in LIVE_MARKERS {
            assert!(
                code.contains(marker),
                "`{marker}` no longer appears in src/ui/sound.rs, so the guards above are \
                 no longer watching for it — update PROBE_MARKERS/LIVE_MARKERS to the new \
                 name"
            );
        }
    }
}
