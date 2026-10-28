//! The Notifications page's bespoke GTK glue (tasks 11.7, 11.15, 13.1; architecture §7;
//! R4.2, R5.1, R5.2).
//!
//! # Two halves on one page
//!
//! The page's upper half is **runtime-only** (R5.2): it reads or changes live state of
//! the notifd notification daemon over D-Bus and touches no file, so nothing is staged,
//! nothing is dirty and the Apply/Reset chrome is never involved. The declarative row
//! framework ([`super::page`]) renders store-backed settings only, so — like the Sound
//! and Network pages — this half renders directly from a GTK-free core model,
//! [`NotificationsModel`], which owns every decision and every command (and is
//! unit-tested headlessly there). This module only lays out widgets, forwards the
//! user's actions to the model and re-renders from it.
//!
//! The lower half is notifd's **staged** settings from `~/.config/notifd/config` (task
//! 11.15): popup timeouts, history size, critical-popup cap, hold time and the 2FA copy
//! switch. Those are ordinary store-backed settings, so they *are* framework rows:
//! [`page_rows`] describes them, the window launches a [`super::page::SettingsPage`] for
//! them (which owns the Apply/Reset wiring and the dirty marker) and hands its widget to
//! [`build`], which only places it under a heading. The rows exist only when the startup
//! load built a [`NotifdConfigModel`] — notifd ≥ 1.1 runs and the file is readable —
//! otherwise the runtime half stands alone.
//!
//! # What it shows
//!
//! A status line (reading / not running / running), then:
//!
//! - **Do Not Disturb** — a switch applied immediately with `SetDnd`;
//! - **Notifications** — how many notifications the history holds;
//! - **Popups held back** — whether an application currently inhibits popups;
//! - **Notification center** — whether the centre is open, as the daemon last reported;
//! - **Open notification center** — runs the eww script `notif-center open`, exactly
//!   what the bar click and `SUPER+N` do (task 13.1; see below). Shown only when that
//!   script and `eww` were detected (R4.2); never notifd's `SetCenter`, which opens no
//!   window and only mutes popups;
//! - **Clear all…** — opens a confirmation dialog; only "Clear all" in that dialog
//!   issues `Clear` ([`NotificationsModel::request_clear`] /
//!   [`NotificationsModel::confirm_clear`]).
//!
//! When the daemon is installed but not running (it starts with the first notification
//! an application sends) the page says so, offers a "Check again" button beside the
//! status line (like the Sound page's refresh button) and makes the controls insensitive,
//! because every call would fail until it starts. A failed action shows its reason
//! under the status line, not only in the journal. That is not the "greyed-out stub" R4.2
//! forbids: R4.2 is about a *missing* application, which hides the whole page (the
//! category gate, `super::category`); here the application is installed and the page
//! states why it cannot act right now. The next page entry re-reads the daemon.
//!
//! # Nothing runs while the window builds the page (task 9.26 rule)
//!
//! [`build`] issues no command (placing the already-built staged rows spawns nothing
//! either — they render from the store): the model starts in
//! [`DaemonView::NotQueried`] and the page shows a placeholder. The first read happens
//! when the page becomes the visible stack child, through the window's deferred
//! page-entry hook (which runs it from the GTK idle queue, see
//! `wire_page_entry` in [`super::window`]), and again on every later entry —
//! so a wedged session bus can never stall the populate pass against the R8.1 budget,
//! and a state change made elsewhere (a keybind toggling do-not-disturb) shows up the
//! next time the page is opened. The widget handlers connected in [`build`] only run
//! when the user acts.
//!
//! `tests/page_entry_deferral.rs` guards this at the source level for this page and
//! its runtime-backed siblings: nothing reachable from [`build`] may spawn a command,
//! and the page-entry `refresh` must reach the live read.
//!
//! # Synchronous on the GTK main thread (deliberate), except opening the centre
//!
//! Each `busctl` call is short-lived, so — matching the Sound and Network pages — they
//! run synchronously on the GTK main thread through the real
//! [`CommandRunner`], bounded by its 5 s timeout.
//!
//! Opening the notification centre is the exception (task 13.1). The eww script waits
//! for the eww daemon for a few seconds and bounds each of its own calls at 5 s, so a
//! successful open can take longer than the runner's usual timeout; it runs under its
//! own [`CENTER_SCRIPT_TIMEOUT`](crate::core::notifications::CENTER_SCRIPT_TIMEOUT) on a
//! worker thread ([`run_off_main_thread`]), exactly like the Display page's
//! laptop-display toggle (tasks 10.3, 10.7). [`CenterOpenState`] carries the pieces of
//! that pattern: while the script runs the button is insensitive and says it is
//! opening, so a second click cannot start a second run; each open carries a
//! generation number so a late result can never finish a newer open; and a watchdog
//! re-enables the button if no result comes back within [`CENTER_OPEN_WATCHDOG`].
//! After the script the daemon's state is re-read and the page shows whether the
//! centre is open from that read. The state lives as long as the window, not the page,
//! because a Refresh rebuilds the page — and its model — while the script may still be
//! running.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{
    AlertDialog, Align, Box as GtkBox, Button, Label, Orientation, ScrolledWindow, Switch, Widget,
    Window, gio, glib,
};

use crate::core::model::SettingId;
use crate::core::notifd::NotifdState;
use crate::core::notifd_config::{NotifdConfigModel, NotifdRule, NotifdSetting};
use crate::core::notifications::{
    DaemonView, NotifdActionError, NotificationsModel, OpenCenterError, OpenCenterOutcome,
};
use crate::system::command::{CommandRunner, SystemCommandRunner};
use crate::ui::row::{RowCapability, RowDescriptor, WidgetKind};
use crate::ui::widgets::{PAGE_MARGIN, ROW_SPACING, SECTION_SPACING, labelled_row, note};
use crate::ui::worker::run_off_main_thread;

/// The confirmation dialog's button index for "Clear all"; index 0 is "Cancel".
const CLEAR_BUTTON: i32 = 1;

/// The "open the notification center" button's label while no open is running.
const OPEN_CENTER_LABEL: &str = "Open notification center";

/// The button's label while the eww script runs (task 13.1); the button is
/// insensitive meanwhile.
const OPENING_CENTER_LABEL: &str = "Opening the notification center…";

/// What a failed open says before its reason (see [`action_error_text`]).
const OPEN_CENTER_FAILED: &str = "The notification center could not be opened";

/// How long the page waits for an open of the notification centre to report back
/// before it gives up and re-enables the button (task 13.1, the task 10.7 pattern).
///
/// One open runs the script, bounded by
/// [`CENTER_SCRIPT_TIMEOUT`](crate::core::notifications::CENTER_SCRIPT_TIMEOUT) (20 s),
/// and one state read bounded by the runner's 5 s, so an open that has not reported
/// back after 30 s is stuck — typically the runner still draining output pipes that a
/// killed script's children hold open — not slow. Nothing is killed when it fires.
const CENTER_OPEN_WATCHDOG: Duration = Duration::from_secs(30);

/// The note the watchdog leaves when an open did not report back in time: the button
/// is usable again, and the shown state may be stale.
const CENTER_WATCHDOG_NOTE: &str = "Opening the notification center did not report back, \
                                    so the button is available again. Open this page again \
                                    to read the current state.";

/// The state of opening the notification centre that must outlive any one
/// Notifications page and its model (task 13.1): whether the eww script is running on
/// the worker, why the last open failed, which page and model to update when the
/// result arrives, and the watchdog that stops waiting for a result that never comes.
///
/// The window creates one for its whole lifetime (through [`Default`]) and hands it to
/// every page it builds ([`build`]); see the module docs for why it cannot live on the
/// page. It is the Display page's `LaptopToggleState` pattern (tasks 10.3, 10.7)
/// applied to this button.
pub(crate) struct CenterOpenState {
    /// Whether an open is running. While `true` the button is insensitive and labelled
    /// as opening, and a new open is refused. Cleared when the result arrives or, at
    /// the latest, when the watchdog fires.
    running: Cell<bool>,
    /// Why the last open failed, shown under the page's status line until the next
    /// open or page entry (R5.5), or `None`.
    error: RefCell<Option<String>>,
    /// The model of the page that currently shows the button: the open's request is
    /// taken from it and its result stored on it. Weak, so a model the window dropped
    /// with its page is simply skipped.
    model: RefCell<Weak<RefCell<NotificationsModel>>>,
    /// The page that currently shows the button, re-rendered when an open starts or
    /// ends. Weak for the same reason.
    page: RefCell<Weak<Inner>>,
    /// The number of the most recently started open. Each open's result and watchdog
    /// carry the number it started with and act only while it is still the current one
    /// *and* [`Self::running`] is set, so neither a late result nor a stale watchdog
    /// can finish a newer open.
    generation: Cell<u64>,
    /// How long an open may run before the watchdog gives up on it:
    /// [`CENTER_OPEN_WATCHDOG`] in the app, chosen per open in tests. Read when an
    /// open starts, so a change affects only opens started afterwards.
    watchdog_interval: Cell<Duration>,
    /// The running open's armed watchdog, aborted when the result arrives first;
    /// `None` when no open is running.
    watchdog: RefCell<Option<glib::JoinHandle<()>>>,
}

impl Default for CenterOpenState {
    /// No open running, no note, no page yet, and the production watchdog interval.
    fn default() -> Self {
        Self::with_watchdog_interval(CENTER_OPEN_WATCHDOG)
    }
}

impl CenterOpenState {
    /// A state with no open running whose watchdog gives up after
    /// `watchdog_interval`.
    fn with_watchdog_interval(watchdog_interval: Duration) -> Self {
        Self {
            running: Cell::new(false),
            error: RefCell::new(None),
            model: RefCell::new(Weak::new()),
            page: RefCell::new(Weak::new()),
            generation: Cell::new(0),
            watchdog_interval: Cell::new(watchdog_interval),
            watchdog: RefCell::new(None),
        }
    }

    /// Makes `model` the one later opens take their request from and store their
    /// result on. [`build`] calls it for every new page, so a result that arrives
    /// after a Refresh rebuilt the page lands on the rebuilt page's model.
    fn attach_model(&self, model: &Rc<RefCell<NotificationsModel>>) {
        self.model.replace(Rc::downgrade(model));
    }

    /// Starts opening the notification centre (R5.2, task 13.1): takes the request from
    /// the attached model, marks the open as running, arms its watchdog and
    /// re-renders the page, then runs the eww script and the state read through
    /// `runner` on a worker thread and returns at once. The result is handled by
    /// [`Self::finish`] on the main thread or, if it does not arrive within the
    /// watchdog interval, the wait is abandoned by [`Self::watchdog_fired`].
    ///
    /// A call while an open is running is ignored — the button is insensitive then,
    /// so only a click already queued could reach it — rather than running the
    /// script twice at once. An open the model refuses (no script, daemon not
    /// running) runs nothing and only shows why.
    fn start(self: &Rc<Self>, runner: impl CommandRunner + Send + 'static) {
        if self.running.get() {
            tracing::debug!(
                "the notification center is already being opened; the new click is ignored"
            );
            return;
        }
        let Some(model) = self.model.borrow().upgrade() else {
            tracing::debug!("no Notifications page is mounted; nothing to open from");
            return;
        };
        let request = model.borrow().open_center_request();
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                self.error.replace(Some(open_error_text(&error)));
                self.rerender_page();
                return;
            }
        };

        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.running.set(true);
        self.error.replace(None);
        self.arm_watchdog(generation);
        self.rerender_page();

        let state = Rc::clone(self);
        run_off_main_thread(
            move || request.run(&runner),
            move |outcome| state.finish(generation, outcome),
        );
    }

    /// Schedules [`Self::watchdog_fired`] for open `generation` after the watchdog
    /// interval, on the calling thread's main context. The timer holds only a
    /// [`Weak`] reference, so it never keeps the state alive on its own.
    fn arm_watchdog(self: &Rc<Self>, generation: u64) {
        let state = Rc::downgrade(self);
        let interval = self.watchdog_interval.get();
        let handle = relm4::spawn_local(async move {
            glib::timeout_future(interval).await;
            if let Some(state) = state.upgrade() {
                state.watchdog_fired(generation);
            }
        });
        // No previous open's handle can be left here: an open only starts when none
        // is running, and every running open ends in `finish` (which takes its handle)
        // or in `watchdog_fired` (which clears it).
        debug_assert!(self.watchdog.borrow().is_none());
        self.watchdog.replace(Some(handle));
    }

    /// Gives up waiting for open `generation`: re-enables the button, leaves
    /// [`CENTER_WATCHDOG_NOTE`], logs at `warn` and re-renders the page. Nothing is
    /// killed or signalled; the result, should it still arrive, is ignored by
    /// [`Self::finish`]. Does nothing when that open is no longer the running one.
    fn watchdog_fired(&self, generation: u64) {
        if !self.is_running(generation) {
            tracing::debug!(
                generation,
                "a notification center watchdog fired after its open had ended; ignored"
            );
            return;
        }
        tracing::warn!(
            interval = ?self.watchdog_interval.get(),
            "opening the notification center did not report back in time; the button is \
             re-enabled, and the result will be ignored if it still arrives (R5.5)"
        );
        self.watchdog.replace(None);
        self.running.set(false);
        self.error.replace(Some(CENTER_WATCHDOG_NOTE.to_owned()));
        self.rerender_page();
    }

    /// Whether open `generation` is the one the page is still waiting for.
    fn is_running(&self, generation: u64) -> bool {
        self.running.get() && self.generation.get() == generation
    }

    /// Handles the result of open `generation` on the main thread: cancels its
    /// watchdog, stores the state read after the script on the attached model
    /// ([`NotificationsModel::finish_open_center`]), keeps a failure for the page to
    /// show, clears the running flag and re-renders the page.
    ///
    /// `outcome` is `None` when the worker ended without one (it panicked); the button
    /// is then re-enabled and the loss logged. A result for an open the page is no
    /// longer waiting for — its watchdog fired — is logged at `info` and otherwise
    /// ignored, its state read included: a newer open or page entry may have read the
    /// daemon since, and the late read cannot be told apart from an older one.
    fn finish(&self, generation: u64, outcome: Option<OpenCenterOutcome>) {
        if !self.is_running(generation) {
            tracing::info!(
                generation,
                current = self.generation.get(),
                ?outcome,
                "opening the notification center reported back after its watchdog gave up \
                 on it; the result is ignored"
            );
            return;
        }
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.abort();
        }
        self.running.set(false);
        let model = self.model.borrow().upgrade();
        let result = match (outcome, model) {
            (Some(outcome), Some(model)) => Some(model.borrow_mut().finish_open_center(outcome)),
            (Some(outcome), None) => {
                tracing::debug!(
                    ?outcome,
                    "the notification center was opened with no Notifications page mounted; \
                     the result has nowhere to be shown"
                );
                None
            }
            (None, _) => {
                tracing::error!(
                    "the notification center worker ended without a result; the button is \
                     available again (R5.5)"
                );
                None
            }
        };
        // The model has already logged any failure; here it is only kept for the page.
        self.error.replace(match result {
            Some(Err(error)) => Some(open_error_text(&error)),
            Some(Ok(())) | None => None,
        });
        self.rerender_page();
    }

    /// Re-renders the page currently showing the button, if the window still has one.
    fn rerender_page(&self) {
        // Upgrade first, so the `page` borrow is released before the render runs.
        let page = self.page.borrow().upgrade();
        if let Some(page) = page {
            page.rerender();
        }
    }
}

/// The line shown for a failed open (R5.5): "The notification center could not be
/// opened: eww is not running." for the script's exit status 1 (task 13.1).
fn open_error_text(error: &OpenCenterError) -> String {
    format!("{OPEN_CENTER_FAILED}: {error}.")
}

/// The mounted Notifications page: the scrollable root plus the handle the window uses
/// to re-read the daemon when the page is shown.
pub(crate) struct NotificationsPage {
    /// The scrollable widget mounted in the window's stack.
    root: ScrolledWindow,
    /// The shared render state; this strong reference keeps it — and thus the widget
    /// handlers, which hold only [`Weak`] references — alive for the life of the page.
    inner: Rc<Inner>,
}

impl NotificationsPage {
    /// The widget to add to the window's stack.
    pub(crate) fn root(&self) -> &ScrolledWindow {
        &self.root
    }

    /// Re-reads the daemon and re-renders the page — called by the window's deferred
    /// page-entry hook, which covers both the first read (see the module docs) and
    /// picking up a change made while the app sat on another page.
    pub(crate) fn refresh(&self) {
        self.inner.refresh();
    }
}

/// The page's model and the widgets its render touches.
///
/// Handlers capture a [`Weak`] to this and upgrade on use, so the widget tree never
/// forms a reference cycle with the closures mounted inside it (the Sound/Network
/// pattern).
struct Inner {
    /// The headless model every decision and command goes through. Shared (weakly)
    /// with [`CenterOpenState`], which stores an open's result on it.
    model: Rc<RefCell<NotificationsModel>>,
    /// The window-lifetime state of opening the notification centre (task 13.1).
    center: Rc<CenterOpenState>,
    /// The one-line daemon status above the controls.
    status: Label,
    /// "Check again", shown beside the status only while the daemon is not running,
    /// so the user can re-read it after starting notifd without leaving the page.
    check_again: Button,
    /// Why the last action failed, shown under the status line; hidden when empty.
    error: Label,
    /// The text [`Self::error`] shows: set by a failed action, cleared by the next
    /// successful one and by every re-read (the state it described may be gone).
    last_error: RefCell<Option<String>>,
    /// The controls, made insensitive as a group while the daemon cannot be reached.
    controls: GtkBox,
    /// The do-not-disturb switch.
    dnd_switch: Switch,
    /// The switch's `notify::active` handler, blocked while rendering so a programmatic
    /// set is never taken for a user toggle. Set once, right after connecting.
    dnd_handler: OnceCell<glib::SignalHandlerId>,
    /// The notification count value.
    count_value: Label,
    /// The "popups held back" value.
    inhibited_value: Label,
    /// Whether the notification centre is open, as last read from the daemon.
    center_value: Label,
    /// "Open notification center": hidden without the eww script or eww, insensitive
    /// and relabelled while an open runs (task 13.1).
    open_button: Button,
}

impl Inner {
    /// Re-reads the daemon through the model and re-renders.
    fn refresh(&self) {
        self.model.borrow_mut().refresh(&SystemCommandRunner::new());
        self.last_error.replace(None);
        self.center.error.replace(None);
        self.rerender();
    }

    /// Re-renders every widget from the model's current view without querying the daemon
    /// (that is [`Self::refresh`]); [`build`] calls it once to paint the not-yet-read state.
    fn rerender(&self) {
        let view = self.model.borrow().view();
        self.status.set_text(&status_text(view));
        self.check_again
            .set_visible(matches!(view, DaemonView::NotRunning));
        let errors: Vec<String> = [
            self.last_error.borrow().clone(),
            self.center.error.borrow().clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        self.error.set_text(&errors.join("\n"));
        self.error.set_visible(!errors.is_empty());
        let (running, state) = match view {
            DaemonView::Running { state, .. } => (true, state),
            DaemonView::NotQueried | DaemonView::NotRunning => (false, None),
        };
        self.controls.set_sensitive(running);
        self.set_switch(state.is_some_and(|state| state.dnd));
        self.count_value.set_text(&count_text(state));
        self.inhibited_value.set_text(&inhibited_text(state));
        self.center_value.set_text(&center_text(state));
        let opening = self.center.running.get();
        self.open_button
            .set_visible(self.model.borrow().can_open_center());
        self.open_button.set_sensitive(!opening);
        self.open_button.set_label(if opening {
            OPENING_CENTER_LABEL
        } else {
            OPEN_CENTER_LABEL
        });
    }

    /// Sets the switch without firing its handler.
    fn set_switch(&self, active: bool) {
        match self.dnd_handler.get() {
            Some(handler) => {
                self.dnd_switch.block_signal(handler);
                self.dnd_switch.set_active(active);
                self.dnd_switch.unblock_signal(handler);
            }
            None => self.dnd_switch.set_active(active),
        }
    }

    /// Records an action's outcome for [`Self::rerender`]: a failure's text, or nothing.
    fn record(&self, what: &str, result: Result<(), NotifdActionError>) {
        self.last_error
            .replace(result.err().map(|error| action_error_text(what, &error)));
    }

    /// The user flipped the switch: apply it at once (R5.2). On failure the model keeps
    /// the previous state (it has already logged why), the re-render moves the switch
    /// back to it, and the reason appears under the status line.
    fn toggle_dnd(&self, active: bool) {
        let result = self
            .model
            .borrow_mut()
            .set_dnd(&SystemCommandRunner::new(), active);
        self.record("Do Not Disturb was not changed", result);
        self.rerender();
    }

    /// The user asked to open the notification center: run the eww script on a worker
    /// thread (task 13.1). The page re-renders when the open starts and again when its
    /// result arrives.
    fn open_center(&self) {
        self.last_error.replace(None);
        self.center.start(SystemCommandRunner::new());
    }

    /// The user answered the confirmation dialog: clear on "Clear all", forget the
    /// request otherwise. After a clear the daemon is re-read so the count shows the
    /// result; a failure is recorded *after* that re-read, which would clear it.
    fn answer_clear(&self, confirmed: bool) {
        if !confirmed {
            self.model.borrow_mut().cancel_clear();
            self.rerender();
            return;
        }
        let runner = SystemCommandRunner::new();
        let result = self.model.borrow_mut().confirm_clear(&runner);
        self.model.borrow_mut().refresh(&runner);
        self.record("The notifications were not cleared", result);
        self.rerender();
    }
}

/// The staged notifd rows (task 11.15), or **no rows** when there is no
/// [`NotifdConfigModel`] — notifd is not running at version 1.1 or later, or its file is
/// missing or not understood, which the startup load has logged at `info`.
///
/// One spin button per integer key, bounded by exactly the range notifd accepts (so the
/// control cannot produce a value the daemon would ignore, R8.3), and a switch for
/// `copy_codes`. Every row is [`RowCapability::Always`]: the model's existence is the
/// whole gate, so the seven rows appear or disappear together. Kept GTK-free so the
/// decision is testable headlessly (R6.2).
pub(crate) fn page_rows(model: Option<&NotifdConfigModel>) -> Vec<RowDescriptor> {
    if model.is_none() {
        return Vec::new();
    }
    NOTIFD_ROWS
        .iter()
        .filter_map(|row| {
            let setting = NotifdSetting::for_id(row.setting)?;
            let widget = match (setting.rule, row.spin_step) {
                (NotifdRule::Integer { min, max, .. }, Some(step)) => WidgetKind::Spin {
                    min: min as f64,
                    max: max as f64,
                    step,
                },
                (NotifdRule::Bool { .. }, None) => WidgetKind::Switch,
                // A table entry whose step does not fit its key's kind is an authoring
                // mistake; the row is dropped and the row-count test fails.
                (NotifdRule::Integer { .. }, None) | (NotifdRule::Bool { .. }, Some(_)) => {
                    return None;
                }
            };
            Some(RowDescriptor::new(
                row.label,
                widget,
                row.setting,
                RowCapability::Always,
            ))
        })
        .collect()
}

/// One staged notifd row of the page (see [`NOTIFD_ROWS`]).
struct NotifdRow {
    /// The setting the row edits.
    setting: SettingId,
    /// The label beside the control.
    label: &'static str,
    /// For an integer key, the step of one spin-button arrow click (Page Up/Down moves
    /// ten steps); `None` for the boolean key, which is a switch.
    spin_step: Option<f64>,
}

/// The staged rows in display order. Half a second per click suits the popup timeouts,
/// a tenth of one the hover hold time, ten entries the history size.
const NOTIFD_ROWS: [NotifdRow; 7] = [
    NotifdRow {
        setting: SettingId::NotifdTimeoutLow,
        label: "Low-urgency popups stay for (ms)",
        spin_step: Some(500.0),
    },
    NotifdRow {
        setting: SettingId::NotifdTimeoutNormal,
        label: "Normal popups stay for (ms)",
        spin_step: Some(500.0),
    },
    NotifdRow {
        setting: SettingId::NotifdTimeoutCritical,
        label: "Critical popups stay for (ms)",
        spin_step: Some(500.0),
    },
    NotifdRow {
        setting: SettingId::NotifdHoldMin,
        label: "Popup stays after the pointer leaves it (ms)",
        spin_step: Some(100.0),
    },
    NotifdRow {
        setting: SettingId::NotifdHistoryMax,
        label: "Notifications kept in the history",
        spin_step: Some(10.0),
    },
    NotifdRow {
        setting: SettingId::NotifdCriticalPerApp,
        label: "Critical popups one app may show at once",
        spin_step: Some(1.0),
    },
    NotifdRow {
        setting: SettingId::NotifdCopyCodes,
        label: "Offer to copy two-factor codes",
        spin_step: None,
    },
];

/// The explanation shown above the staged rows: the unit, what `0` means, and how a
/// change takes effect.
const STAGED_NOTE: &str = "Times are in milliseconds; 0 keeps a popup until it is \
                           dismissed. An app's own timeout always wins. Changes are saved \
                           to ~/.config/notifd/config with Apply and take effect for new \
                           notifications without dropping the ones on screen.";

/// Builds the Notifications page (tasks 11.7, 11.15, 13.1). Runs no command (see the
/// module docs); the returned page must be kept alive by the window, which mounts
/// [`NotificationsPage::root`] and calls [`NotificationsPage::refresh`] on page entry.
///
/// `center_script` is the eww `notif-center` script when it and eww were detected
/// ([`center_script`](crate::core::notifications::center_script)), else `None`, which
/// hides the "open the notification center" button. `center` is the window's
/// long-lived [`CenterOpenState`]; the new page registers itself and its model there,
/// so an open still running from a previous page reports back here.
///
/// `staged` is the root widget of the framework page the window launched for
/// [`page_rows`], or `None` when there are no staged rows; it is placed under the
/// runtime controls with a heading and [`STAGED_NOTE`]. It stays sensitive when the
/// daemon is found not running on page entry: the edits are saved to the file and
/// notifd reads it when it starts.
pub(crate) fn build(
    staged: Option<&Widget>,
    center_script: Option<String>,
    center: Rc<CenterOpenState>,
) -> NotificationsPage {
    let bar_json =
        NotificationsModel::bar_json_path(std::env::var_os("XDG_RUNTIME_DIR").as_deref());

    let content = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    content.set_margin_top(PAGE_MARGIN);
    content.set_margin_bottom(PAGE_MARGIN);
    content.set_margin_start(PAGE_MARGIN);
    content.set_margin_end(PAGE_MARGIN);

    let status = note("");
    status.set_hexpand(true);
    let check_again = Button::with_label("Check again");
    check_again.set_valign(Align::Center);
    let status_row = GtkBox::new(Orientation::Horizontal, ROW_SPACING);
    status_row.append(&status);
    status_row.append(&check_again);
    let error = note("");
    let dnd_switch = Switch::new();
    dnd_switch.set_halign(Align::End);
    dnd_switch.set_valign(Align::Center);
    dnd_switch.set_tooltip_text(Some("Applied immediately"));
    let count_value = Label::new(None);
    let inhibited_value = Label::new(None);
    let center_value = Label::new(None);
    let open_button = Button::with_label(OPEN_CENTER_LABEL);
    open_button.set_halign(Align::Start);
    let clear_button = Button::with_label("Clear all…");
    clear_button.set_halign(Align::Start);

    let controls = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    controls.append(&labelled_row("Do Not Disturb", &dnd_switch));
    controls.append(&labelled_row("Notifications", &count_value));
    controls.append(&labelled_row(
        "Popups held back by an app",
        &inhibited_value,
    ));
    controls.append(&labelled_row("Notification center", &center_value));
    let buttons = GtkBox::new(Orientation::Horizontal, ROW_SPACING);
    buttons.append(&open_button);
    buttons.append(&clear_button);
    controls.append(&buttons);

    content.append(&status_row);
    content.append(&error);
    content.append(&controls);
    if let Some(staged) = staged {
        let heading = Label::new(Some("Popups and history"));
        heading.set_halign(Align::Start);
        heading.set_margin_top(SECTION_SPACING);
        content.append(&heading);
        content.append(&note(STAGED_NOTE));
        // The framework page brings its own page margins; this page already has them.
        staged.set_margin_top(0);
        staged.set_margin_bottom(0);
        staged.set_margin_start(0);
        staged.set_margin_end(0);
        content.append(staged);
    }

    let model = Rc::new(RefCell::new(NotificationsModel::new(
        bar_json,
        center_script,
    )));
    center.attach_model(&model);
    let inner = Rc::new(Inner {
        model,
        center: Rc::clone(&center),
        status,
        check_again: check_again.clone(),
        error,
        last_error: RefCell::new(None),
        controls,
        dnd_switch: dnd_switch.clone(),
        dnd_handler: OnceCell::new(),
        count_value,
        inhibited_value,
        center_value,
        open_button: open_button.clone(),
    });
    center.page.replace(Rc::downgrade(&inner));
    inner.rerender();

    let weak = Rc::downgrade(&inner);
    let handler = dnd_switch.connect_active_notify(move |switch| {
        if let Some(inner) = weak.upgrade() {
            inner.toggle_dnd(switch.is_active());
        }
    });
    // Freshly created, so the cell is empty and `set` cannot fail; ignoring the result
    // keeps this free of a panic path all the same.
    let _ = inner.dnd_handler.set(handler);

    let weak = Rc::downgrade(&inner);
    check_again.connect_clicked(move |_| {
        if let Some(inner) = weak.upgrade() {
            inner.refresh();
        }
    });

    let weak = Rc::downgrade(&inner);
    open_button.connect_clicked(move |_| {
        if let Some(inner) = weak.upgrade() {
            inner.open_center();
        }
    });

    let weak = Rc::downgrade(&inner);
    clear_button.connect_clicked(move |button| {
        if let Some(inner) = weak.upgrade() {
            inner.model.borrow_mut().request_clear();
            confirm_clear(button.upcast_ref(), Rc::downgrade(&inner));
        }
    });

    let root = ScrolledWindow::new();
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.set_child(Some(&content));

    NotificationsPage { root, inner }
}

/// Asks the user to confirm clearing every notification (a plain `gtk::AlertDialog` —
/// no libadwaita, no custom CSS, R2.1) and hands the answer to the model. Closing the
/// dialog any other way counts as "Cancel".
fn confirm_clear(anchor: &Widget, inner: Weak<Inner>) {
    let dialog = AlertDialog::builder()
        .message("Clear all notifications?")
        .detail("Every notification in the history is removed. This cannot be undone.")
        .buttons(["Cancel", "Clear all"])
        .cancel_button(0)
        .default_button(0)
        .modal(true)
        .build();
    let parent = anchor.root().and_downcast::<Window>();
    dialog.choose(parent.as_ref(), None::<&gio::Cancellable>, move |answer| {
        if let Some(inner) = inner.upgrade() {
            inner.answer_clear(answer.is_ok_and(|index| index == CLEAR_BUTTON));
        }
    });
}

/// The status line for `view`.
fn status_text(view: DaemonView) -> String {
    match view {
        DaemonView::NotQueried => "Reading the notification state…".to_string(),
        DaemonView::NotRunning => "The notification daemon (notifd) is not running. It starts \
                                   with the next notification; open this page again then."
            .to_string(),
        DaemonView::Running {
            state: None,
            version,
        } => format!(
            "notifd{} is running, but its state could not be read.",
            version_suffix(version)
        ),
        DaemonView::Running {
            state: Some(_),
            version,
        } => format!("notifd{} is running.", version_suffix(version)),
    }
}

/// The line shown under the status for a failed action: what did not happen, then why
/// (the error's own text, which names the exit status and quotes the stderr excerpt).
fn action_error_text(what: &str, error: &NotifdActionError) -> String {
    format!("{what}: {error}.")
}

/// ` <version>` when known, else nothing.
fn version_suffix(version: Option<crate::core::notifd::NotifdVersion>) -> String {
    version.map_or_else(String::new, |version| format!(" {version}"))
}

/// The notification count, or a dash when unknown.
fn count_text(state: Option<NotifdState>) -> String {
    state.map_or_else(|| "—".to_string(), |state| state.count.to_string())
}

/// Whether the notification centre is open, or a dash when unknown.
fn center_text(state: Option<NotifdState>) -> String {
    match state {
        Some(NotifdState {
            center_open: true, ..
        }) => "Open".to_string(),
        Some(_) => "Closed".to_string(),
        None => "—".to_string(),
    }
}

/// Whether popups are held back, or a dash when unknown.
fn inhibited_text(state: Option<NotifdState>) -> String {
    match state {
        Some(NotifdState {
            inhibited: true, ..
        }) => "Yes".to_string(),
        Some(_) => "No".to_string(),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::notifd::NotifdVersion;

    // The texts are pure functions of the model's view, so they are checked without a
    // display; the widgets themselves need a GTK runtime the test suite does not have.

    #[test]
    fn the_status_line_names_each_daemon_state() {
        assert!(status_text(DaemonView::NotQueried).starts_with("Reading"));
        assert!(status_text(DaemonView::NotRunning).contains("not running"));
        let state = NotifdState {
            dnd: false,
            count: 3,
            inhibited: false,
            center_open: false,
        };
        assert_eq!(
            status_text(DaemonView::Running {
                version: Some(NotifdVersion::STATE_AND_RELOAD),
                state: Some(state),
            }),
            "notifd 1.1 is running."
        );
        assert_eq!(
            status_text(DaemonView::Running {
                version: None,
                state: None,
            }),
            "notifd is running, but its state could not be read."
        );
    }

    #[test]
    fn a_failed_action_message_explains_itself() {
        let error = NotifdActionError::Failed {
            code: Some(1),
            stderr: Some("Call failed: no such name".to_string()),
        };
        assert_eq!(
            action_error_text("Do Not Disturb was not changed", &error),
            "Do Not Disturb was not changed: the notification daemon call exited with \
             status 1: Call failed: no such name."
        );
    }

    #[test]
    fn there_are_no_staged_rows_without_a_model() {
        // Task 11.15: without a model (notifd < 1.1, or no readable file) the page has
        // only its runtime controls.
        assert!(page_rows(None).is_empty());
    }

    #[test]
    fn the_staged_rows_match_the_daemon_ranges() {
        // Task 11.15: with a model, the page has seven well-formed rows whose spin
        // bounds are exactly notifd's ranges.
        use crate::core::notifd::{NotifdStatus, NotifdVersion};

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config");
        std::fs::write(&path, "").expect("an empty file seeds every default");
        let model = NotifdConfigModel::offer(
            NotifdStatus::Running {
                version: Some(NotifdVersion::STATE_AND_RELOAD),
            },
            path,
        )
        .expect("offered");
        let rows = page_rows(Some(&model));
        assert_eq!(
            rows.iter().map(|row| row.setting).collect::<Vec<_>>(),
            vec![
                SettingId::NotifdTimeoutLow,
                SettingId::NotifdTimeoutNormal,
                SettingId::NotifdTimeoutCritical,
                SettingId::NotifdHoldMin,
                SettingId::NotifdHistoryMax,
                SettingId::NotifdCriticalPerApp,
                SettingId::NotifdCopyCodes,
            ]
        );
        for row in &rows {
            assert!(row.is_well_formed(), "{:?}", row.setting);
            let setting = NotifdSetting::for_id(row.setting).expect("a notifd key");
            match (setting.rule, &row.widget) {
                (
                    NotifdRule::Integer { min, max, .. },
                    WidgetKind::Spin {
                        min: lo,
                        max: hi,
                        step,
                    },
                ) => {
                    assert_eq!((*lo, *hi), (min as f64, max as f64), "{}", setting.key);
                    assert!(*step >= 1.0, "{}", setting.key);
                }
                (NotifdRule::Bool { .. }, WidgetKind::Switch) => {}
                (rule, widget) => panic!("{rule:?} rendered as {widget:?}"),
            }
        }
        assert!(STAGED_NOTE.contains("0 keeps a popup until it is dismissed"));
    }

    #[test]
    fn unknown_values_render_as_a_dash() {
        assert_eq!(count_text(None), "—");
        assert_eq!(inhibited_text(None), "—");
        assert_eq!(center_text(None), "—");
        let state = NotifdState {
            dnd: true,
            count: 22,
            inhibited: true,
            center_open: false,
        };
        assert_eq!(count_text(Some(state)), "22");
        assert_eq!(inhibited_text(Some(state)), "Yes");
        assert_eq!(center_text(Some(state)), "Closed");
        assert_eq!(
            center_text(Some(NotifdState {
                center_open: true,
                ..state
            })),
            "Open"
        );
        assert_eq!(
            inhibited_text(Some(NotifdState {
                inhibited: false,
                ..state
            })),
            "No"
        );
    }
}

#[cfg(test)]
mod center_tests {
    //! Opening the notification centre off the main thread (task 13.1), exercised
    //! without widgets: [`CenterOpenState`] touches widgets only through a registered
    //! page, so these tests attach just a model and drive their own GLib main context
    //! in place of the GTK main loop — the same approach as the Display page's laptop
    //! toggle tests.

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Duration;

    use gtk4::glib::MainContext;

    use super::{CENTER_WATCHDOG_NOTE, CenterOpenState};
    use crate::core::notifd::{NotifdMethod, NotifdState, control_command};
    use crate::core::notifications::{DaemonView, NotificationsModel, center_open_command};
    use crate::system::command::{
        Command, CommandError, CommandOutput, CommandRunner, MockCommandRunner,
    };
    use crate::ui::worker::drive_until;

    /// A placeholder path for the detected `notif-center` script; nothing is executed.
    const CENTER_SCRIPT: &str = "/test/config/eww/scripts/notif-center";

    /// A successful command printing `stdout`.
    fn ok(stdout: &str) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput::fake_with_streams(0, stdout, ""))
    }

    /// The `GetState` reply with the centre open or closed.
    fn state_reply(center_open: bool) -> Result<CommandOutput, CommandError> {
        ok(&format!("bubb false 3 false {center_open}\n"))
    }

    /// A model that read a running notifd 1.1 with the centre closed and has the
    /// script, shared the way the page shares it.
    fn shared_model() -> Rc<RefCell<NotificationsModel>> {
        let mut model = NotificationsModel::new(None, Some(CENTER_SCRIPT.to_string()));
        model.refresh(&MockCommandRunner::with_outcomes([
            ok("b true\n"),
            ok("ssss \"notifd\" \"dotfiles\" \"1.1\" \"1.3\"\n"),
            state_reply(false),
        ]));
        Rc::new(RefCell::new(model))
    }

    /// The centre-open flag `model` shows.
    fn center_open(model: &RefCell<NotificationsModel>) -> bool {
        match model.borrow().view() {
            DaemonView::Running {
                state: Some(NotifdState { center_open, .. }),
                ..
            } => center_open,
            other => panic!("unexpected view {other:?}"),
        }
    }

    /// The script-then-read sequence of one open (task 13.1).
    fn open_sequence() -> Vec<Command> {
        vec![
            center_open_command(CENTER_SCRIPT),
            control_command(NotifdMethod::GetState),
        ]
    }

    /// A runner that records through a shared [`MockCommandRunner`] and then, for the
    /// script only, blocks until the test releases it — a script still running on the
    /// worker while the test inspects the main thread.
    struct GatedRunner {
        /// Records every command and answers from its canned outcomes.
        recorder: Arc<MockCommandRunner>,
        /// Receives one message per release of the script.
        gate: Receiver<()>,
    }

    impl CommandRunner for GatedRunner {
        fn run(&self, command: &Command) -> Result<CommandOutput, CommandError> {
            let output = self.recorder.run(command);
            if command.program() == CENTER_SCRIPT {
                self.gate
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the test releases the script");
            }
            output
        }
    }

    /// A gated runner answering `outcomes` in order, its recorder, and the sender that
    /// releases the script.
    fn gated_runner(
        outcomes: [Result<CommandOutput, CommandError>; 2],
    ) -> (GatedRunner, Arc<MockCommandRunner>, Sender<()>) {
        let recorder = Arc::new(MockCommandRunner::with_outcomes(outcomes));
        let (release, gate) = mpsc::channel();
        let runner = GatedRunner {
            recorder: Arc::clone(&recorder),
            gate,
        };
        (runner, recorder, release)
    }

    /// A [`CommandRunner`] forwarding to a shared mock, so a test can move the runner
    /// into [`CenterOpenState::start`] and still inspect what it recorded.
    struct ArcRunner(Arc<MockCommandRunner>);

    impl CommandRunner for ArcRunner {
        fn run(&self, command: &Command) -> Result<CommandOutput, CommandError> {
            self.0.run(command)
        }
    }

    /// A state with `model` attached and the given watchdog interval.
    fn attached_state(
        model: &Rc<RefCell<NotificationsModel>>,
        interval: Duration,
    ) -> Rc<CenterOpenState> {
        let state = Rc::new(CenterOpenState::with_watchdog_interval(interval));
        state.attach_model(model);
        state
    }

    #[test]
    fn a_second_click_while_the_script_runs_is_refused() {
        // Task 13.1 acceptance: `start` returns while the script is still running on
        // the worker; meanwhile the open is marked running (the button insensitive)
        // and a second click issues nothing. When the result arrives the model shows
        // the centre as the read after the script reported it.
        let model = shared_model();
        let state = attached_state(&model, Duration::from_secs(600));
        let (runner, recorder, release) = gated_runner([ok(""), state_reply(true)]);

        let context = MainContext::new();
        context
            .with_thread_default(|| {
                state.start(runner);
                drive_until(&context, || recorder.recorded().len() == 1);
                assert!(state.running.get(), "the button is disabled");
                assert!(
                    model.try_borrow_mut().is_ok(),
                    "no model borrow is held while the script runs"
                );
                assert!(!center_open(&model), "nothing stored before the read");

                let second = Arc::new(MockCommandRunner::new());
                state.start(ArcRunner(Arc::clone(&second)));
                assert!(second.recorded().is_empty(), "the second click ran nothing");
                assert!(state.running.get());

                release.send(()).expect("the script is waiting");
                drive_until(&context, || !state.running.get());
            })
            .expect("the test thread owns its main context");

        assert!(center_open(&model), "shown from the read after the script");
        assert_eq!(*state.error.borrow(), None);
        assert_eq!(recorder.recorded(), open_sequence());
        assert!(
            recorder
                .recorded()
                .iter()
                .all(|command| !command.to_string().contains("SetCenter")),
            "the daemon flag is only ever set by the script"
        );
    }

    #[test]
    fn exit_status_1_leaves_a_note_naming_eww() {
        let model = shared_model();
        let state = attached_state(&model, Duration::from_secs(600));
        let (runner, recorder, release) =
            gated_runner([Ok(CommandOutput::fake(1)), state_reply(false)]);
        release.send(()).expect("pre-release the script");

        let context = MainContext::new();
        context
            .with_thread_default(|| {
                state.start(runner);
                drive_until(&context, || !state.running.get());
            })
            .expect("the test thread owns its main context");

        assert_eq!(
            state.error.borrow().as_deref(),
            Some("The notification center could not be opened: eww is not running.")
        );
        assert!(!center_open(&model));
        assert_eq!(recorder.recorded(), open_sequence());
    }

    #[test]
    fn an_open_the_model_refuses_runs_nothing_and_says_why() {
        // A daemon seen not running: refused synchronously, no worker, no command.
        let model = Rc::new(RefCell::new(NotificationsModel::new(
            None,
            Some(CENTER_SCRIPT.to_string()),
        )));
        let state = attached_state(&model, Duration::from_secs(600));
        let runner = Arc::new(MockCommandRunner::new());
        state.start(ArcRunner(Arc::clone(&runner)));
        assert!(!state.running.get());
        assert_eq!(
            state.error.borrow().as_deref(),
            Some(
                "The notification center could not be opened: the notification daemon is \
                 not running."
            )
        );
        assert!(runner.recorded().is_empty());
    }

    #[test]
    fn an_open_that_never_reports_back_re_enables_the_button_and_a_late_result_is_ignored() {
        // The watchdog ends the wait: the button is usable again with the note, a newer
        // open can start, and the first open's late result — the centre read as open —
        // neither finishes the newer open nor reaches the model.
        let model = shared_model();
        let state = attached_state(&model, Duration::from_millis(50));
        let (runner_a, recorder_a, release_a) = gated_runner([ok(""), state_reply(true)]);
        let (runner_b, recorder_b, release_b) = gated_runner([ok(""), state_reply(false)]);

        let context = MainContext::new();
        context
            .with_thread_default(|| {
                state.start(runner_a);
                let generation_a = state.generation.get();
                drive_until(&context, || !state.running.get());
                assert_eq!(state.error.borrow().as_deref(), Some(CENTER_WATCHDOG_NOTE));
                drive_until(&context, || recorder_a.recorded().len() == 1);

                // B must outlive everything below however slowly the machine runs. (The
                // field is set directly: a `#[cfg(test)]` helper above `build` would cut
                // the page-entry guard's view of the module short.)
                state.watchdog_interval.set(Duration::from_secs(600));
                state.start(runner_b);
                drive_until(&context, || recorder_b.recorded().len() == 1);
                assert!(state.running.get(), "B is running");

                // A's result, replayed directly and then for real.
                state.finish(generation_a, None);
                assert!(state.running.get(), "B is still running");
                release_a.send(()).expect("A's script is still waiting");
                drive_until(&context, || Rc::strong_count(&state) == 2);
                assert!(state.running.get(), "B is still running");
                assert!(!center_open(&model), "A's late read was not stored");

                release_b.send(()).expect("B's script is still waiting");
                drive_until(&context, || Rc::strong_count(&state) == 1);
            })
            .expect("the test thread owns its main context");

        assert!(!state.running.get(), "B finished with its own result");
        assert_eq!(*state.error.borrow(), None);
        assert!(!center_open(&model), "B's read (closed) is shown");
        assert_eq!(recorder_a.recorded(), open_sequence());
        assert_eq!(recorder_b.recorded(), open_sequence());
    }

    #[test]
    fn a_result_lands_on_the_model_of_the_page_that_replaced_the_first() {
        // A Refresh rebuilds the page and its model while the script runs; the result
        // is stored on the newly attached model.
        let first = shared_model();
        let state = attached_state(&first, Duration::from_secs(600));
        let (runner, _recorder, release) = gated_runner([ok(""), state_reply(true)]);

        let context = MainContext::new();
        context
            .with_thread_default(|| {
                state.start(runner);
                let second = shared_model();
                state.attach_model(&second);
                release.send(()).expect("the script is waiting");
                drive_until(&context, || !state.running.get());
                assert!(
                    center_open(&second),
                    "the rebuilt page's model got the read"
                );
                assert!(!center_open(&first));
            })
            .expect("the test thread owns its main context");
    }
}
