//! The Notifications page's bespoke GTK glue (task 11.7; architecture §7; R4.2, R5.2).
//!
//! # Why this page is bespoke, not declarative
//!
//! Every control on the page is **runtime-only** (R5.2): it reads or changes live
//! state of the notifd notification daemon over D-Bus and touches no file, so nothing
//! is staged, nothing is dirty and the Apply/Reset chrome is never involved. The
//! declarative row framework ([`super::page`]) renders store-backed settings only, so —
//! like the Sound and Network pages — this page renders directly from a GTK-free core
//! model, [`NotificationsModel`], which owns every decision and every command (and is
//! unit-tested headlessly there). This module only lays out widgets, forwards the
//! user's actions to the model and re-renders from it.
//!
//! # What it shows
//!
//! A status line (reading / not running / running), then:
//!
//! - **Do Not Disturb** — a switch applied immediately with `SetDnd`;
//! - **Notifications** — how many notifications the history holds;
//! - **Popups held back** — whether an application currently inhibits popups;
//! - **Open notification center** — `SetCenter open`;
//! - **Clear all…** — opens a confirmation dialog; only "Clear all" in that dialog
//!   issues `Clear` ([`NotificationsModel::request_clear`] /
//!   [`NotificationsModel::confirm_clear`]).
//!
//! When the daemon is installed but not running (it starts with the first notification
//! an application sends) the page says so, offers a "Check again" button beside the
//! status line (like the Sound page's rescan) and makes the controls insensitive,
//! because every call would fail until it starts. A failed action shows its reason
//! under the status line, not only in the journal. That is not the "greyed-out stub" R4.2
//! forbids: R4.2 is about a *missing* application, which hides the whole page (the
//! category gate, `super::category`); here the application is installed and the page
//! states why it cannot act right now. The next page entry re-reads the daemon.
//!
//! # Nothing runs while the window builds the page (task 9.26 rule)
//!
//! [`build`] issues no command: the model starts in
//! [`DaemonView::NotQueried`] and the page shows a placeholder. The first read happens
//! when the page becomes the visible stack child, through the window's deferred
//! page-entry hook (which runs it from the GTK idle queue, see
//! `wire_deferred_page_entry` in [`super::window`]), and again on every later entry —
//! so a wedged session bus can never stall the populate pass against the R8.1 budget,
//! and a state change made elsewhere (a keybind toggling do-not-disturb) shows up the
//! next time the page is opened. The widget handlers connected in [`build`] only run
//! when the user acts.
//!
//! `tests/page_entry_deferral.rs` guards this at the source level for this page and
//! its runtime-backed siblings: nothing reachable from [`build`] may spawn a command,
//! and the page-entry `refresh` must reach the live read.
//!
//! # Synchronous on the GTK main thread (deliberate)
//!
//! Each `busctl` call is short-lived, so — matching the Sound and Network pages — they
//! run synchronously on the GTK main thread through the real
//! [`CommandRunner`](crate::system::command::CommandRunner), bounded by its 5 s timeout.

use std::cell::{OnceCell, RefCell};
use std::rc::{Rc, Weak};

use gtk4::prelude::*;
use gtk4::{
    AlertDialog, Align, Box as GtkBox, Button, Label, Orientation, ScrolledWindow, Switch, Widget,
    Window, gio, glib,
};

use crate::core::notifd::NotifdState;
use crate::core::notifications::{DaemonView, NotifdActionError, NotificationsModel};
use crate::system::command::SystemCommandRunner;

/// Outer margin, in pixels, around the page content.
const PAGE_MARGIN: i32 = 18;

/// Vertical spacing, in pixels, between sections and between rows.
const SECTION_SPACING: i32 = 12;

/// Horizontal spacing, in pixels, between a row's label and its control or value.
const ROW_SPACING: i32 = 8;

/// The confirmation dialog's button index for "Clear all"; index 0 is "Cancel".
const CLEAR_BUTTON: i32 = 1;

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
    /// The headless model every decision and command goes through.
    model: RefCell<NotificationsModel>,
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
}

impl Inner {
    /// Re-reads the daemon through the model and re-renders.
    fn refresh(&self) {
        self.model.borrow_mut().refresh(&SystemCommandRunner::new());
        self.last_error.replace(None);
        self.render();
    }

    /// Renders every widget from the model's current view.
    fn render(&self) {
        let view = self.model.borrow().view();
        self.status.set_text(&status_text(view));
        self.check_again
            .set_visible(matches!(view, DaemonView::NotRunning));
        match self.last_error.borrow().as_deref() {
            Some(text) => {
                self.error.set_text(text);
                self.error.set_visible(true);
            }
            None => self.error.set_visible(false),
        }
        let (running, state) = match view {
            DaemonView::Running { state, .. } => (true, state),
            DaemonView::NotQueried | DaemonView::NotRunning => (false, None),
        };
        self.controls.set_sensitive(running);
        self.set_switch(state.is_some_and(|state| state.dnd));
        self.count_value.set_text(&count_text(state));
        self.inhibited_value.set_text(&inhibited_text(state));
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

    /// Records an action's outcome for [`Self::render`]: a failure's text, or nothing.
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
        self.render();
    }

    /// The user asked to open the notification center.
    fn open_center(&self) {
        let result = self
            .model
            .borrow_mut()
            .open_center(&SystemCommandRunner::new());
        self.record("The notification center was not opened", result);
        self.render();
    }

    /// The user answered the confirmation dialog: clear on "Clear all", forget the
    /// request otherwise. After a clear the daemon is re-read so the count shows the
    /// result; a failure is recorded *after* that re-read, which would clear it.
    fn answer_clear(&self, confirmed: bool) {
        if !confirmed {
            self.model.borrow_mut().cancel_clear();
            self.render();
            return;
        }
        let runner = SystemCommandRunner::new();
        let result = self.model.borrow_mut().confirm_clear(&runner);
        self.model.borrow_mut().refresh(&runner);
        self.record("The notifications were not cleared", result);
        self.render();
    }
}

/// Builds the Notifications page (task 11.7). Runs no command (see the module docs);
/// the returned page must be kept alive by the window, which mounts
/// [`NotificationsPage::root`] and calls [`NotificationsPage::refresh`] on page entry.
pub(crate) fn build() -> NotificationsPage {
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
    let open_button = Button::with_label("Open notification center");
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
    let buttons = GtkBox::new(Orientation::Horizontal, ROW_SPACING);
    buttons.append(&open_button);
    buttons.append(&clear_button);
    controls.append(&buttons);

    content.append(&status_row);
    content.append(&error);
    content.append(&controls);

    let inner = Rc::new(Inner {
        model: RefCell::new(NotificationsModel::new(bar_json)),
        status,
        check_again: check_again.clone(),
        error,
        last_error: RefCell::new(None),
        controls,
        dnd_switch: dnd_switch.clone(),
        dnd_handler: OnceCell::new(),
        count_value,
        inhibited_value,
    });
    inner.render();

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

/// A row: a `label` taking the free space and its `control` on the right.
fn labelled_row(label: &str, control: &impl IsA<Widget>) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, ROW_SPACING);
    let label = Label::new(Some(label));
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    row.append(&label);
    row.append(control);
    row
}

/// A left-aligned, wrapping informational label.
fn note(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.set_halign(Align::Start);
    label.set_wrap(true);
    label.set_xalign(0.0);
    label
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
    fn a_failed_action_names_what_did_not_happen_and_why() {
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
    fn unknown_values_render_as_a_dash() {
        assert_eq!(count_text(None), "—");
        assert_eq!(inhibited_text(None), "—");
        let state = NotifdState {
            dnd: true,
            count: 22,
            inhibited: true,
            center_open: false,
        };
        assert_eq!(count_text(Some(state)), "22");
        assert_eq!(inhibited_text(Some(state)), "Yes");
        assert_eq!(
            inhibited_text(Some(NotifdState {
                inhibited: false,
                ..state
            })),
            "No"
        );
    }
}
