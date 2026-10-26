//! The Display page's bespoke GTK glue (tasks 6.1, 10.3, 10.4, 10.7, 11.8, 11.9;
//! architecture §7; R2.3, R4.2, R4.4, R5.2, R8.3).
//!
//! # Why this page is bespoke, not declarative
//!
//! Every other §6 page is a list of fixed [`RowDescriptor`](super::row::RowDescriptor)s
//! rendered by the declarative framework ([`super::page`]). The Display page cannot
//! use it for two reasons: its controls are **per-monitor and known only at runtime**
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
//! A non-laptop monitor's resolution/refresh/scale/position edits, and its enable
//! switch, are **staged** into the model and written to `monitors.conf` on Apply —
//! switching an external output off adds the `disabled,1` pair to its record and
//! switching it on removes it (task 10.4); the page reports each through the shared
//! `on_changed` callback so the window's Apply/Reset chrome lights up (as the
//! declarative pages do). The laptop panel's enable control is
//! **runtime-only**: toggling it runs the dotfiles' `scripts/hypr-toggle-laptop-display`
//! right away, between two reads of the compositor's monitor list (task 11.8), never
//! staging and never marking the page dirty. The switch is re-drawn from the state
//! *read* afterwards, so a toggle that did not take snaps back and shows why
//! underneath. The control exists only when detection found the script (R4.2). Its
//! mode/scale, by contrast, are staged like any monitor's (the single-source gotcha,
//! analysis §6.2).
//!
//! # The laptop toggle runs off the main thread (task 10.3)
//!
//! Unlike the other runtime controls (Sound, Network, Notifications), whose commands
//! return quickly and run on the main thread, the toggle script can take seconds: it
//! ends by relaunching the eww bars, which can restart the eww daemon. Its 5 s runner
//! timeout bounds the script itself, **not** the wait for that relaunch — on a
//! timeout the runner kills only the script's shell and then keeps reading the
//! captured output until the orphaned `launch-eww-bars` closes the pipes it
//! inherited. On the main thread that froze the window for the script's whole
//! duration. So the page uses the model's three-step toggle: the request is taken
//! from the model on the main thread, [`LaptopToggleRequest::run`] issues the
//! commands on a worker ([`run_off_main_thread`]), and the outcome comes back to the
//! main thread, where [`DisplayModel::finish_laptop_toggle`] stores the read state
//! and the page rebuilds. Meanwhile the switch shows the requested position but is
//! insensitive, with a note saying the switch is in progress, so a second toggle
//! cannot start while the script runs.
//!
//! Two alternatives were rejected. Killing the script's whole process group on a
//! timeout would SIGKILL `launch-eww-bars` mid-restart and leave the session without
//! bars. A longer per-command timeout for the script would not help at all: the
//! window would still be frozen while it ran, only for longer.
//!
//! Apply stays clickable while a toggle is in flight. A Display Apply writes
//! `monitors.conf` and runs `hyprctl reload`, which can now interleave with the
//! script's `hl.monitor` call and bar relaunch — impossible before, when the toggle
//! blocked the main thread. The race is accepted rather than prevented: the switch
//! is drawn from the second read, never from an assumed state, so it shows whatever
//! the panel ended up as; a conflict reload that replaces the model mid-toggle is
//! handled by storing the read by output name; and the Lua monitor loader in
//! `hyprland.lua` keeps the docked panel state across reloads (analysis §8.1), so a
//! reload does not undo the toggle.
//!
//! The in-flight flag and the last toggle's error live in a [`LaptopToggleState`] the
//! window owns, not on the page or the model: a Refresh rebuilds this page and may
//! replace the model while the script runs, and either would otherwise forget that a
//! toggle is still running and offer the switch again. When the outcome arrives it
//! is stored on whichever model is current, and whichever page is current is rebuilt.
//!
//! # A toggle that never reports back (task 10.7)
//!
//! The worker's wait has no bound of its own: after the runner's 5 s timeout kills
//! the script's shell, the runner keeps draining the captured output until every
//! process holding the inherited pipes closes them, and a wedged grandchild (a
//! `launch-eww-bars` that never exits) never does. So every toggle arms a watchdog
//! on the main loop when it starts. If no outcome has arrived when it fires, the
//! page stops waiting: the switch is re-enabled at the panel state last *read* (the
//! model's, not the requested position), a note says the toggle did not report
//! back, and a `warn` names the output and the interval. Nothing is killed or
//! signalled — the stuck process is the bar launcher, and killing it would leave the
//! session without bars — so the abandoned worker simply stays blocked until its
//! pipes close. An outcome that arrives first cancels the watchdog.
//!
//! Each toggle carries a generation number, and only the outcome of the toggle the
//! page is still waiting for is stored. An outcome that turns up after its watchdog
//! fired is logged at `info` and dropped as a whole, its read state included: by
//! then the user may have started a newer toggle, whose own reads are the ones to
//! show, and the late read may be older than state stored since (by that newer
//! toggle or by a Refresh). The page cannot tell which read is fresher, so it keeps
//! the one it has; the next Refresh reads the compositor again.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Frame, Orientation, ScrolledWindow, Switch};

use crate::core::display::{
    DisplayModel, LaptopToggleError, LaptopToggleOutcome, LaptopToggleRequest, MonitorView,
    StageRefusal,
};
use crate::system::command::{CommandRunner, SystemCommandRunner};
use crate::ui::widgets::{PAGE_MARGIN, SECTION_SPACING, build_dropdown, labelled_row, note};
use crate::ui::worker::run_off_main_thread;

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
        // so their notes must not survive into the re-rendered page. A toggle still
        // running stays marked as running: its outcome has not arrived yet.
        self.inner.laptop.error.replace(None);
        self.inner.stage_error.replace(None);
        self.inner.rebuild();
    }
}

/// How long the page waits for a laptop toggle's outcome before it gives up and
/// re-enables the switch (task 10.7).
///
/// One toggle issues at most three commands, each bounded by the runner's 5 s
/// timeout, so a toggle that has not reported back after twice that total is stuck
/// in the runner's drain of the script's output (see the module docs), not slow.
const LAPTOP_TOGGLE_WATCHDOG: Duration = Duration::from_secs(30);

/// The runtime laptop toggle's state that must outlive any one Display page and any
/// one Display model (task 10.3): whether a toggle is running on the worker, why the
/// last one did not take, and which page to rebuild when an outcome arrives — plus
/// the watchdog that stops waiting for an outcome that never comes (task 10.7).
///
/// The window creates one for its whole lifetime (through [`Default`], which arms
/// the production watchdog interval) and hands it to every page it builds
/// ([`build`]). See the module docs for why the flag cannot live on the page or on
/// the model.
pub(crate) struct LaptopToggleState {
    /// The panel state the running toggle asked for (`true` for on), or `None` when
    /// no toggle is running. While `Some`, the switch is drawn at this position and
    /// insensitive, and a new toggle is refused.
    ///
    /// It stays `Some` until the toggle's outcome arrives or, at the latest, until
    /// the watchdog fires after [`Self::watchdog_interval`] (task 10.7). Neither
    /// Refresh nor Reset clears it: the script may still be running then.
    pending: Cell<Option<bool>>,
    /// Why the last laptop toggle did not take, shown under the switch until the
    /// next toggle, Reset or Apply (R5.5), or `None` after a successful one.
    error: RefCell<Option<String>>,
    /// The page that currently shows the switch, rebuilt when a toggle starts or
    /// finishes. Weak, so a page the window dropped (a Refresh rebuilds the page)
    /// is simply skipped.
    page: RefCell<Weak<Inner>>,
    /// The number of the most recently started toggle. Each toggle's outcome and
    /// watchdog carry the number it started with, and act only while it is still
    /// the current one *and* [`Self::pending`] is `Some` — so neither a late outcome
    /// nor a stale watchdog can finish a newer toggle (task 10.7).
    generation: Cell<u64>,
    /// How long a toggle may run before the watchdog gives up on it:
    /// [`LAPTOP_TOGGLE_WATCHDOG`] in the app, chosen per toggle in tests. Read when
    /// a toggle starts, so a change affects only toggles started afterwards.
    watchdog_interval: Cell<Duration>,
    /// The running toggle's armed watchdog, aborted when the outcome arrives first
    /// so a toggle that finished in time leaves no timer behind; `None` when no
    /// toggle is running.
    watchdog: RefCell<Option<glib::JoinHandle<()>>>,
}

impl Default for LaptopToggleState {
    /// No toggle running, no note, no page yet, and the production watchdog interval.
    fn default() -> Self {
        Self::with_watchdog_interval(LAPTOP_TOGGLE_WATCHDOG)
    }
}

impl LaptopToggleState {
    /// A state with no toggle running whose watchdog gives up on a toggle after
    /// `watchdog_interval` — [`LAPTOP_TOGGLE_WATCHDOG`] through [`Default`]; tests
    /// pass a short interval so they need not wait half a minute.
    fn with_watchdog_interval(watchdog_interval: Duration) -> Self {
        Self {
            pending: Cell::new(None),
            error: RefCell::new(None),
            page: RefCell::new(Weak::new()),
            generation: Cell::new(0),
            watchdog_interval: Cell::new(watchdog_interval),
            watchdog: RefCell::new(None),
        }
    }

    /// Changes the watchdog interval for toggles started from now on, so a test can
    /// let one toggle's watchdog fire quickly and give the next one no practical
    /// deadline.
    #[cfg(test)]
    fn set_watchdog_interval(&self, interval: Duration) {
        self.watchdog_interval.set(interval);
    }

    /// Starts the runtime laptop toggle of monitor `index` towards `enable` (R5.2):
    /// takes the request from the shared `model`, marks the toggle as running, arms
    /// its watchdog and rebuilds the page, then runs the script and its reads
    /// through `runner` on a worker thread and returns at once. The outcome is
    /// handled by [`Self::finish`] on the main thread, or — if it does not arrive
    /// within the watchdog interval — the wait is abandoned by
    /// [`Self::watchdog_fired`] (task 10.7).
    ///
    /// A toggle the model refuses outright ([`LaptopToggleError::Unavailable`]) runs
    /// nothing and only shows why; without a model there is no switch to update. A
    /// call while a toggle is already running is ignored — the switch is
    /// insensitive then, so only a signal already queued could reach it — rather
    /// than running the pure-toggle script twice at once.
    ///
    /// Runtime-only: it never stages or changes dirty state, so the window's
    /// Apply/Reset chrome is not told about it.
    fn start(
        self: &Rc<Self>,
        model: &Rc<RefCell<Option<DisplayModel>>>,
        index: usize,
        enable: bool,
        runner: impl CommandRunner + Send + 'static,
    ) {
        if self.pending.get().is_some() {
            tracing::debug!("a laptop-display toggle is already running; the new one is ignored");
            return;
        }
        let request = model
            .borrow()
            .as_ref()
            .map(|model| model.laptop_toggle_request(index, enable));
        let request: LaptopToggleRequest = match request {
            Some(Ok(request)) => request,
            Some(Err(error)) => {
                self.error.replace(Some(toggle_error_note(&error)));
                self.rebuild_page();
                return;
            }
            None => {
                self.error.replace(None);
                self.rebuild_page();
                return;
            }
        };

        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.pending.set(Some(request.enable()));
        self.error.replace(None);
        self.arm_watchdog(generation, request.output().to_owned());
        self.rebuild_page();

        let state = Rc::clone(self);
        let model = Rc::clone(model);
        run_off_main_thread(
            move || request.run(&runner),
            move |outcome| state.finish(generation, &model, outcome),
        );
    }

    /// Schedules [`Self::watchdog_fired`] for toggle `generation` of `output` after
    /// the watchdog interval, on the calling thread's main context (the GTK main
    /// loop in the app, a test's own context in tests).
    ///
    /// The timer holds only a [`Weak`] reference, so it never keeps the state alive
    /// on its own.
    fn arm_watchdog(self: &Rc<Self>, generation: u64, output: String) {
        let state = Rc::downgrade(self);
        let interval = self.watchdog_interval.get();
        let handle = relm4::spawn_local(async move {
            glib::timeout_future(interval).await;
            if let Some(state) = state.upgrade() {
                state.watchdog_fired(generation, &output);
            }
        });
        // No previous toggle's handle can be left here: a toggle only starts when
        // none is running, and every running toggle ends either in `finish`, which
        // takes its handle, or in `watchdog_fired`, which clears it.
        debug_assert!(self.watchdog.borrow().is_none());
        self.watchdog.replace(Some(handle));
    }

    /// Gives up waiting for toggle `generation` of `output` (task 10.7): clears the
    /// running flag so the switch is re-enabled at the model's last read state,
    /// leaves a note saying the toggle did not report back, logs at `warn` and
    /// rebuilds the page. The worker is left alone — nothing is killed or
    /// signalled — and its outcome, should it ever arrive, is ignored by
    /// [`Self::finish`].
    ///
    /// Does nothing when that toggle is no longer the one running: its outcome
    /// arrived first (which also aborts this timer, so this is only a safeguard) or
    /// a newer toggle has started since.
    fn watchdog_fired(&self, generation: u64, output: &str) {
        if !self.is_running(generation) {
            tracing::debug!(
                generation,
                "a laptop-display toggle watchdog fired after its toggle had ended; ignored"
            );
            return;
        }
        tracing::warn!(
            %output,
            interval = ?self.watchdog_interval.get(),
            "the laptop-display toggle did not report back in time; the switch is \
             re-enabled at the last state read, and the toggle's outcome will be ignored \
             if it still arrives (R5.5)"
        );
        // Only now, with this toggle known to be the running one, is the handle
        // its own; a stale watchdog must never discard a newer toggle's handle.
        self.watchdog.replace(None);
        self.pending.set(None);
        self.error.replace(Some(WATCHDOG_NOTE.to_owned()));
        self.rebuild_page();
    }

    /// Whether toggle `generation` is the one the page is still waiting for: the
    /// most recently started toggle, and not yet finished or given up on.
    fn is_running(&self, generation: u64) -> bool {
        self.pending.get().is_some() && self.generation.get() == generation
    }

    /// Handles the outcome of toggle `generation` on the main thread: cancels its
    /// watchdog, stores the read state on the current model
    /// ([`DisplayModel::finish_laptop_toggle`]), keeps any error for the switch's
    /// note, clears the running flag and rebuilds the current page.
    ///
    /// `outcome` is `None` when the worker ended without one (it panicked); the
    /// switch is then re-enabled at the last state read before the toggle, and the
    /// loss is logged — the commands, if they ran, were logged by the runner.
    ///
    /// An outcome for a toggle the page is no longer waiting for — its watchdog
    /// fired, whether or not a newer toggle has started since — is logged at `info`
    /// and otherwise ignored: it neither stores its read state, nor sets a note,
    /// nor touches the running flag (see the module docs for why the read is
    /// dropped too).
    fn finish(
        &self,
        generation: u64,
        model: &RefCell<Option<DisplayModel>>,
        outcome: Option<LaptopToggleOutcome>,
    ) {
        if !self.is_running(generation) {
            tracing::info!(
                generation,
                current = self.generation.get(),
                ?outcome,
                "a laptop-display toggle reported back after its watchdog gave up on it; \
                 the outcome is ignored"
            );
            return;
        }
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.abort();
        }
        self.pending.set(None);
        let result = match outcome {
            Some(outcome) => model
                .borrow_mut()
                .as_mut()
                .map(|model| model.finish_laptop_toggle(outcome)),
            None => {
                tracing::error!(
                    "the laptop-display toggle worker ended without a result; the switch \
                     shows the last state read (R5.5)"
                );
                None
            }
        };
        // The model has already logged any failure; here it is only kept for the
        // rebuild to show under the switch.
        self.error.replace(match result {
            Some(Err(error)) => Some(toggle_error_note(&error)),
            Some(Ok(())) | None => None,
        });
        self.rebuild_page();
    }

    /// Rebuilds the page currently showing the switch, if the window still has one.
    fn rebuild_page(&self) {
        // Upgrade first, so the `page` borrow is released before the rebuild runs.
        let page = self.page.borrow().upgrade();
        if let Some(page) = page {
            page.rebuild();
        }
    }
}

/// The note the watchdog leaves under the laptop switch when a toggle did not report
/// back in time (task 10.7). It tells the user that the switch may be stale — the
/// toggle may still have switched the panel afterwards — and how to read it again.
const WATCHDOG_NOTE: &str = "The laptop display toggle did not report back, so the switch \
                             shows the last state read. Refresh to read it again.";

/// The note shown under the laptop switch for a toggle that did not take (R5.5).
fn toggle_error_note(error: &LaptopToggleError) -> String {
    format!("The laptop display was not switched: {error}.")
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
    /// The window-lifetime state of the runtime laptop toggle (task 10.3): whether
    /// one is running and why the last one failed.
    laptop: Rc<LaptopToggleState>,
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

        let editable = model.records_editable();
        for index in 0..model.monitor_count() {
            // `monitor` is `Some` for every index below the count; going through it
            // keeps the rebuild free of a panic path all the same (task 9.24).
            if let Some(view) = model.monitor(index) {
                self.content
                    .append(&self.build_section(index, view, editable));
            }
        }
    }

    /// Builds the section of monitor `index` from its `view`: a titled frame with its
    /// enable control and, when enabled and `editable` (the model's
    /// [`DisplayModel::records_editable`]), the resolution/refresh/scale/position
    /// drop-downs. `index` is what the controls' handlers hand back to the model.
    fn build_section(
        self: &Rc<Self>,
        index: usize,
        view: MonitorView<'_>,
        editable: bool,
    ) -> Frame {
        let title = format!("{} — {}", view.name(), view.description());
        let frame = Frame::new(Some(title.trim_end_matches(" — ")));

        let section = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
        section.set_margin_top(SECTION_SPACING);
        section.set_margin_bottom(SECTION_SPACING);
        section.set_margin_start(SECTION_SPACING);
        section.set_margin_end(SECTION_SPACING);

        // The enable control. A laptop panel's is the runtime toggle (R5.2), shown
        // only when the dotfiles script behind it was detected (R4.2). A non-laptop's
        // is a staged monitors.conf edit (only when the file is editable): the
        // `disabled,1` pair on its record (task 10.4).
        if view.is_laptop() {
            if view.laptop_toggle_available() {
                section.append(&self.build_laptop_toggle(index, view));
                if self.laptop.pending.get().is_some() {
                    section.append(&note("Switching the laptop display…"));
                } else if let Some(error) = self.laptop.error.borrow().as_deref() {
                    section.append(&note(error));
                }
            }
        } else if editable {
            section.append(&self.build_enable_switch(index, view));
        }

        // The mode/scale/position drop-downs appear only while the output is enabled
        // and the file is editable — there is nothing meaningful to configure on a
        // disabled or unwritable output.
        if editable && view.effective_enabled() {
            section.append(&self.build_resolution_row(index, view));
            let refresh_rate_options = view.refresh_rate_options();
            if !refresh_rate_options.is_empty() {
                section.append(&self.build_refresh_rate_row(index, view, refresh_rate_options));
            }
            section.append(&self.build_scale_row(index, view));
            section.append(&self.build_position_row(index, view));
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
    /// toggle script (task 11.8) on a worker thread (task 10.3), never staging or
    /// touching `monitors.conf`. Its position is the panel's last *read* state —
    /// except while a toggle runs, when it shows the requested state and is
    /// insensitive until the re-read arrives.
    fn build_laptop_toggle(self: &Rc<Self>, index: usize, view: MonitorView<'_>) -> GtkBox {
        let switch = Switch::new();
        switch.set_halign(Align::End);
        switch.set_valign(Align::Center);
        match self.laptop.pending.get() {
            Some(requested) => {
                switch.set_active(requested);
                switch.set_sensitive(false);
            }
            None => switch.set_active(view.laptop_enabled()),
        }

        let weak = Rc::downgrade(self);
        switch.connect_active_notify(move |switch| {
            if let Some(inner) = weak.upgrade() {
                inner.toggle_laptop(index, switch.is_active());
            }
        });

        labelled_row("Enabled", &switch)
    }

    /// A non-laptop monitor's staged enable switch: turning it off stages the
    /// `disabled,1` pair on the output's record, turning it on stages removing it (see
    /// [`DisplayModel::stage_enabled`], task 10.4). The rebuild then hides or shows the
    /// output's mode/scale rows; Reset discards the staged change, and a refused one
    /// snaps the switch back with the reason underneath.
    fn build_enable_switch(self: &Rc<Self>, index: usize, view: MonitorView<'_>) -> GtkBox {
        let switch = Switch::new();
        switch.set_halign(Align::End);
        switch.set_valign(Align::Center);
        switch.set_active(view.effective_enabled());

        let weak = Rc::downgrade(self);
        switch.connect_active_notify(move |switch| {
            if let Some(inner) = weak.upgrade() {
                inner.stage_enabled(index, switch.is_active());
            }
        });

        labelled_row("Enabled", &switch)
    }

    /// The resolution drop-down row.
    fn build_resolution_row(self: &Rc<Self>, index: usize, view: MonitorView<'_>) -> GtkBox {
        let options = view.resolution_options();
        let selected = position_of(&options, &view.effective_resolution());
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
    fn build_refresh_rate_row(
        self: &Rc<Self>,
        index: usize,
        view: MonitorView<'_>,
        options: Vec<String>,
    ) -> GtkBox {
        let selected = view
            .effective_refresh()
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
    fn build_scale_row(self: &Rc<Self>, index: usize, view: MonitorView<'_>) -> GtkBox {
        let options = view.scale_options();
        let selected = position_of(&options, &view.effective_scale());
        let weak = Rc::downgrade(self);
        let dropdown = build_dropdown(&options, selected, move |scale| {
            if let Some(inner) = weak.upgrade() {
                inner.stage_scale(index, scale);
            }
        });
        labelled_row("Scale", &dropdown)
    }

    /// The position drop-down row.
    fn build_position_row(self: &Rc<Self>, index: usize, view: MonitorView<'_>) -> GtkBox {
        let options = view.position_options();
        let selected = position_of(&options, &view.effective_position());
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

    /// Starts the runtime laptop toggle (R5.2) through the real system runner, on a
    /// worker thread ([`LaptopToggleState::start`], task 10.3).
    fn toggle_laptop(self: &Rc<Self>, index: usize, enable: bool) {
        self.laptop
            .start(&self.model, index, enable, SystemCommandRunner::new());
    }
}

/// Builds the Display page over the shared `model`, reporting staged edits through
/// `on_changed` (task 6.1).
///
/// `laptop` is the window's long-lived laptop-toggle state (task 10.3); the new page
/// registers itself there as the one to rebuild when a running toggle finishes, so
/// a toggle started on a page a Refresh has since replaced still updates the switch
/// the user sees.
///
/// The returned [`DisplayPage`] must be kept alive by the window: it owns the strong
/// reference to the render state whose handlers keep the model wired. The window
/// mounts [`DisplayPage::root`] in the stack and calls [`DisplayPage::rerender`] after
/// a Reset or a committed Apply.
pub(crate) fn build(
    model: Rc<RefCell<Option<DisplayModel>>>,
    on_changed: Rc<dyn Fn()>,
    laptop: Rc<LaptopToggleState>,
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
        laptop: Rc::clone(&laptop),
        stage_error: RefCell::new(None),
    });
    laptop.page.replace(Rc::downgrade(&inner));
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

#[cfg(test)]
mod tests {
    //! The page's laptop-toggle bookkeeping, exercised without widgets (tasks 10.3,
    //! 10.7).
    //!
    //! Building the page needs an initialised GTK and so a display server, which the
    //! test suite does not have; [`LaptopToggleState`] does not touch widgets unless
    //! a page is registered, so these tests drive it directly, with their own GLib
    //! main context standing in for the GTK main loop.

    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Duration;

    use gtk4::glib::MainContext;

    use super::{LaptopToggleState, WATCHDOG_NOTE};
    use crate::core::display::DisplayModel;
    use crate::system::command::{
        Command, CommandError, CommandOutput, CommandRunner, MockCommandRunner,
    };
    use crate::ui::worker::drive_until;

    /// A placeholder path for the detected toggle script; nothing is executed.
    const TOGGLE_SCRIPT: &str = "/test/dotfiles/scripts/hypr-toggle-laptop-display";

    /// A `hyprctl monitors all -j` reply listing one laptop panel, `eDP-1`, on or off.
    fn monitors_json(enabled: bool) -> String {
        format!(
            r#"[{{"name": "eDP-1", "description": "Test panel", "width": 2560,
                "height": 1440, "refreshRate": 60.0, "x": 0, "y": 0, "scale": 1.0,
                "disabled": {}, "availableModes": ["2560x1440@60.00Hz"]}}]"#,
            !enabled
        )
    }

    /// A successful monitors reply for the panel in state `enabled`.
    fn probe_reply(enabled: bool) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput::fake_with_streams(
            0,
            &monitors_json(enabled),
            "",
        ))
    }

    /// The shared model the window would hold: one panel, read as on at load, with
    /// the toggle script detected.
    fn shared_model(dir: &Path) -> Rc<RefCell<Option<DisplayModel>>> {
        let conf = dir.join("monitors.conf");
        std::fs::write(&conf, "monitor=,preferred,auto,1\n").expect("write monitors.conf");
        let load = MockCommandRunner::with_outcomes([probe_reply(true)]);
        let model = DisplayModel::load(&load, conf, Some(TOGGLE_SCRIPT.into()))
            .expect("the probe yields a model");
        Rc::new(RefCell::new(Some(model)))
    }

    /// The panel's switch position as the page would draw it from `model`.
    fn panel_enabled(model: &RefCell<Option<DisplayModel>>) -> bool {
        model
            .borrow()
            .as_ref()
            .and_then(|model| model.monitor(0).map(|view| view.laptop_enabled()))
            .expect("the panel is listed")
    }

    /// A runner that records through a shared [`MockCommandRunner`] and then, for
    /// the toggle script only, blocks until the test releases it — a script still
    /// running on the worker while the test inspects the main thread.
    struct GatedRunner {
        /// Records every command and answers from its canned outcomes.
        recorder: Arc<MockCommandRunner>,
        /// Receives one message per release of the script.
        gate: Receiver<()>,
    }

    impl CommandRunner for GatedRunner {
        fn run(&self, command: &Command) -> Result<CommandOutput, CommandError> {
            let output = self.recorder.run(command);
            if command.program() == TOGGLE_SCRIPT {
                self.gate
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the test releases the script");
            }
            output
        }
    }

    /// A gated runner answering `outcomes` in order, its recorder, and the sender
    /// that releases the script.
    fn gated_runner(
        outcomes: [Result<CommandOutput, CommandError>; 3],
    ) -> (GatedRunner, Arc<MockCommandRunner>, Sender<()>) {
        let recorder = Arc::new(MockCommandRunner::with_outcomes(outcomes));
        let (release, gate) = mpsc::channel();
        let runner = GatedRunner {
            recorder: Arc::clone(&recorder),
            gate,
        };
        (runner, recorder, release)
    }

    /// The read–script–read sequence of a toggle that ran the script (task 11.8).
    fn toggle_sequence() -> Vec<Command> {
        vec![
            Command::new("hyprctl").args(["monitors", "all", "-j"]),
            Command::new(TOGGLE_SCRIPT),
            Command::new("hyprctl").args(["monitors", "all", "-j"]),
        ]
    }

    #[test]
    fn the_toggle_result_arrives_through_the_main_loop_while_the_ui_thread_stays_free() {
        // Task 10.3 acceptance: `start` returns while the script is still running
        // on the worker; meanwhile the main thread can borrow the model mutably (no
        // borrow is held across the run), the toggle is marked running, and a second
        // toggle is refused without issuing a command. The result reaches the model
        // only once the main loop delivers it, and then the switch shows the read.
        let dir = tempfile::tempdir().expect("temp dir");
        let model = shared_model(dir.path());
        let state = Rc::new(LaptopToggleState::default());
        let (runner, recorder, release) = gated_runner([
            probe_reply(true),
            Ok(CommandOutput::fake(0)),
            probe_reply(false),
        ]);

        let context = MainContext::new();
        context
            .with_thread_default(|| {
                state.start(&model, 0, false, runner);

                // The script is running on the worker (it has been issued and is
                // blocked) while this thread carries on.
                drive_until(&context, || recorder.recorded().len() == 2);
                assert_eq!(state.pending.get(), Some(false), "the switch is disabled");
                assert!(
                    model.try_borrow_mut().is_ok(),
                    "the main thread holds no model borrow while the script runs"
                );
                assert!(panel_enabled(&model), "nothing stored before the re-read");

                // A second toggle while the first runs is ignored.
                state.start(&model, 0, true, MockCommandRunner::new());
                assert_eq!(state.pending.get(), Some(false));

                release.send(()).expect("the script is waiting");
                drive_until(&context, || state.pending.get().is_none());
            })
            .expect("the test thread owns its main context");

        assert!(!panel_enabled(&model), "the switch shows the re-read state");
        assert_eq!(*state.error.borrow(), None);
        assert_eq!(recorder.recorded(), toggle_sequence());
        assert!(
            !model.borrow().as_ref().expect("model").is_dirty(),
            "the runtime toggle never stages"
        );
    }

    #[test]
    fn an_unchanged_panel_re_enables_the_switch_with_the_reason() {
        // The 11.8 semantics survive the hop: the second read decides, so a panel
        // still on after the script is an error shown under the re-enabled switch,
        // which stays at the read state.
        let dir = tempfile::tempdir().expect("temp dir");
        let model = shared_model(dir.path());
        let state = Rc::new(LaptopToggleState::default());
        let (runner, recorder, release) = gated_runner([
            probe_reply(true),
            Ok(CommandOutput::fake(1)),
            probe_reply(true),
        ]);
        release.send(()).expect("pre-release the script");

        let context = MainContext::new();
        context
            .with_thread_default(|| {
                state.start(&model, 0, false, runner);
                drive_until(&context, || state.pending.get().is_none());
            })
            .expect("the test thread owns its main context");

        assert!(panel_enabled(&model), "the switch stays on, as read");
        assert_eq!(
            state.error.borrow().as_deref(),
            Some(
                "The laptop display was not switched: the panel is still on after running \
                 the toggle script; the toggle script exited with status 1."
            )
        );
        assert_eq!(recorder.recorded(), toggle_sequence());
    }

    #[test]
    fn an_outcome_lands_on_the_model_that_replaced_the_one_it_started_from() {
        // A Refresh can swap in a freshly loaded model while the script runs. The
        // outcome is stored by output name on whichever model is current, so the
        // rebuilt page shows the toggle's read rather than the refresh's older one.
        let dir = tempfile::tempdir().expect("temp dir");
        let model = shared_model(dir.path());
        let state = Rc::new(LaptopToggleState::default());
        let (runner, _recorder, release) = gated_runner([
            probe_reply(true),
            Ok(CommandOutput::fake(0)),
            probe_reply(false),
        ]);

        let context = MainContext::new();
        context
            .with_thread_default(|| {
                state.start(&model, 0, false, runner);
                let replacement = shared_model(dir.path()).borrow_mut().take();
                *model.borrow_mut() = replacement;
                assert!(
                    panel_enabled(&model),
                    "the replacement read the panel as on"
                );
                release.send(()).expect("the script is waiting");
                drive_until(&context, || state.pending.get().is_none());
            })
            .expect("the test thread owns its main context");

        assert!(
            !panel_enabled(&model),
            "the toggle's read reached the new model"
        );
    }

    #[test]
    fn a_toggle_the_model_refuses_runs_nothing_and_says_why() {
        // Without a detected script the switch is hidden; a caller that tries anyway
        // gets the refusal as a note, synchronously, with no worker and no command.
        let dir = tempfile::tempdir().expect("temp dir");
        let conf = dir.path().join("monitors.conf");
        let load = MockCommandRunner::with_outcomes([probe_reply(true)]);
        let model = Rc::new(RefCell::new(DisplayModel::load(&load, conf, None)));
        let state = Rc::new(LaptopToggleState::default());
        let runner = Arc::new(MockCommandRunner::new());

        state.start(&model, 0, false, ArcRunner(Arc::clone(&runner)));

        assert_eq!(state.pending.get(), None);
        assert!(
            state
                .error
                .borrow()
                .as_deref()
                .is_some_and(|note| note.contains("not available")),
            "got {:?}",
            state.error.borrow()
        );
        assert!(runner.recorded().is_empty());
    }

    /// A [`CommandRunner`] forwarding to a shared mock, so a test can move the runner
    /// into [`LaptopToggleState::start`] and still inspect what it recorded.
    struct ArcRunner(Arc<MockCommandRunner>);

    impl CommandRunner for ArcRunner {
        fn run(&self, command: &Command) -> Result<CommandOutput, CommandError> {
            self.0.run(command)
        }
    }

    /// The watchdog interval of the tests that let it fire: long enough for the
    /// worker to reach the blocked script call, short enough to keep the tests fast.
    const SHORT_WATCHDOG: Duration = Duration::from_millis(50);

    /// Iterates `context` until every toggle outcome `state` is waiting on has been
    /// delivered — late ones included — by watching `state`'s strong count: each
    /// outcome handler holds one strong reference until it has run, and the watchdog
    /// holds none, so the count returns to the test's own reference exactly when the
    /// last outcome has been handled.
    fn drive_until_every_outcome_arrived(context: &MainContext, state: &Rc<LaptopToggleState>) {
        drive_until(context, || Rc::strong_count(state) == 1);
    }

    /// The read–script prefix of [`toggle_sequence`]: what a toggle whose script never
    /// returns has issued — and nothing else, so in particular no `kill`.
    fn stuck_sequence() -> Vec<Command> {
        toggle_sequence()[..2].to_vec()
    }

    #[test]
    fn a_toggle_that_never_reports_back_re_enables_the_switch_after_the_watchdog() {
        // Task 10.7 acceptance: the script never returns (the gate stays shut until
        // the end), so only the watchdog can end the wait. It re-enables the switch at
        // the last state *read* (on, as loaded — not the requested off), leaves the
        // note and logs a `warn` naming the output and the interval. The runner saw
        // just the first read and the script: nothing was killed or signalled (the
        // page holds no `ProcessSignaller`, and a `kill` would be a recorded command).
        // When the script is finally released its outcome — a read of "off" — is
        // ignored, since nothing waits for it any more.
        let events = crate::testing::capture_logs(|| {
            let dir = tempfile::tempdir().expect("temp dir");
            let model = shared_model(dir.path());
            let state = Rc::new(LaptopToggleState::with_watchdog_interval(SHORT_WATCHDOG));
            let (runner, recorder, release) = gated_runner([
                probe_reply(true),
                Ok(CommandOutput::fake(0)),
                probe_reply(false),
            ]);

            let context = MainContext::new();
            context
                .with_thread_default(|| {
                    state.start(&model, 0, false, runner);
                    drive_until(&context, || state.pending.get().is_none());

                    assert!(
                        panel_enabled(&model),
                        "the switch is back at the read state"
                    );
                    assert_eq!(state.error.borrow().as_deref(), Some(WATCHDOG_NOTE));
                    // On a busy machine the watchdog can fire before the worker has
                    // even reached the script, so wait for it to block there first.
                    drive_until(&context, || recorder.recorded().len() == 2);
                    assert_eq!(recorder.recorded(), stuck_sequence());

                    release.send(()).expect("the script is still waiting");
                    drive_until_every_outcome_arrived(&context, &state);
                })
                .expect("the test thread owns its main context");

            assert!(
                panel_enabled(&model),
                "the late read of \"off\" was not stored"
            );
            assert_eq!(state.error.borrow().as_deref(), Some(WATCHDOG_NOTE));
            assert_eq!(state.pending.get(), None);
            assert_eq!(
                recorder.recorded(),
                toggle_sequence(),
                "no command besides the toggle"
            );
        });

        let warning = events
            .iter()
            .find(|event| event.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("no warn event in {events:?}"));
        assert!(
            warning.message.contains("did not report back"),
            "{warning:?}"
        );
        assert!(warning.fields.contains("output=eDP-1"), "{warning:?}");
        assert!(warning.fields.contains("interval=50ms"), "{warning:?}");
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::INFO
                    && event.message.contains("after its watchdog gave up")),
            "the late outcome is logged: {events:?}"
        );
    }

    #[test]
    fn a_late_outcome_never_finishes_a_newer_toggle() {
        // Task 10.7 acceptance: toggle A never reports back and its watchdog fires;
        // the user toggles again (B, also blocked in its script). A's outcome then
        // arrives — first replayed directly, deterministically, then for real once
        // A's script is released — and must leave B running, the model unchanged and
        // the note empty. B's own outcome still finishes B.
        let dir = tempfile::tempdir().expect("temp dir");
        let model = shared_model(dir.path());
        let state = Rc::new(LaptopToggleState::with_watchdog_interval(SHORT_WATCHDOG));
        // A reads the panel off after its script; B's script leaves it on.
        let (runner_a, recorder_a, release_a) = gated_runner([
            probe_reply(true),
            Ok(CommandOutput::fake(0)),
            probe_reply(false),
        ]);
        let (runner_b, recorder_b, release_b) = gated_runner([
            probe_reply(true),
            Ok(CommandOutput::fake(1)),
            probe_reply(true),
        ]);

        let context = MainContext::new();
        context
            .with_thread_default(|| {
                state.start(&model, 0, false, runner_a);
                let generation_a = state.generation.get();
                drive_until(&context, || state.pending.get().is_none());
                assert_eq!(state.error.borrow().as_deref(), Some(WATCHDOG_NOTE));

                // B must outlive everything below however slowly the machine runs,
                // so its watchdog gets no practical deadline.
                state.set_watchdog_interval(Duration::from_secs(600));
                state.start(&model, 0, false, runner_b);
                drive_until(&context, || recorder_b.recorded().len() == 2);
                assert_eq!(state.pending.get(), Some(false), "B is running");

                // A's outcome, as its worker would deliver it: the panel read off.
                let late_a = model
                    .borrow()
                    .as_ref()
                    .expect("model")
                    .laptop_toggle_request(0, false)
                    .expect("the toggle is available")
                    .run(&MockCommandRunner::with_outcomes([
                        probe_reply(true),
                        Ok(CommandOutput::fake(0)),
                        probe_reply(false),
                    ]));
                state.finish(generation_a, &model, Some(late_a));
                state.finish(generation_a, &model, None);
                assert_eq!(state.pending.get(), Some(false), "B is still running");
                assert!(panel_enabled(&model), "A's read was not stored");
                assert_eq!(*state.error.borrow(), None, "A left no note");
                assert!(
                    state.watchdog.borrow().is_some(),
                    "B's watchdog is still armed"
                );

                // A's real outcome arrives while B still runs (its strong reference
                // is the only one released before B is).
                release_a.send(()).expect("A's script is still waiting");
                drive_until(&context, || Rc::strong_count(&state) == 2);
                assert_eq!(state.pending.get(), Some(false), "B is still running");
                assert!(panel_enabled(&model), "A's real read was not stored");

                release_b.send(()).expect("B's script is still waiting");
                drive_until_every_outcome_arrived(&context, &state);
            })
            .expect("the test thread owns its main context");

        assert_eq!(state.pending.get(), None, "B finished with its own outcome");
        assert!(panel_enabled(&model), "B's read (still on) is shown");
        assert!(
            state
                .error
                .borrow()
                .as_deref()
                .is_some_and(|note| note.contains("still on after running the toggle script")),
            "B's error is shown: {:?}",
            state.error.borrow()
        );
        assert_eq!(recorder_a.recorded(), toggle_sequence());
        assert_eq!(recorder_b.recorded(), toggle_sequence());
    }

    #[test]
    fn an_outcome_before_the_watchdog_cancels_it() {
        // The outcome arrives well inside the interval: its timer is destroyed with
        // it, and once the interval has passed there is still no note and the panel
        // shows the toggle's read — a stale watchdog cannot re-open a finished toggle.
        let interval = Duration::from_millis(500);
        let dir = tempfile::tempdir().expect("temp dir");
        let model = shared_model(dir.path());
        let state = Rc::new(LaptopToggleState::with_watchdog_interval(interval));
        let (runner, _recorder, release) = gated_runner([
            probe_reply(true),
            Ok(CommandOutput::fake(0)),
            probe_reply(false),
        ]);
        release.send(()).expect("pre-release the script");

        let context = MainContext::new();
        context
            .with_thread_default(|| {
                state.start(&model, 0, false, runner);
                let timer = state
                    .watchdog
                    .borrow()
                    .as_ref()
                    .map(|handle| handle.source().clone())
                    .expect("the watchdog is armed while the toggle runs");
                drive_until(&context, || state.pending.get().is_none());
                assert!(timer.is_destroyed(), "the outcome cancelled the watchdog");
                assert!(state.watchdog.borrow().is_none());

                let after_interval = std::time::Instant::now() + interval + interval / 2;
                drive_until(&context, || std::time::Instant::now() >= after_interval);
            })
            .expect("the test thread owns its main context");

        assert_eq!(*state.error.borrow(), None, "no watchdog note");
        assert_eq!(state.pending.get(), None);
        assert!(!panel_enabled(&model), "the toggle's read stays");
    }
}
