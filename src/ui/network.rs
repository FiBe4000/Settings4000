//! The Network page's bespoke GTK glue (task 6.9; architecture §7; R3.1, R4.2).
//!
//! # Why this page is bespoke, not declarative
//!
//! The declarative row framework ([`super::page`]) renders store-backed settings and
//! stages edits for Apply. The Network page has none: it is **read-only** in v1 and
//! runtime-backed (R3.1) — its content is the live NetworkManager status, nothing is
//! staged, nothing is dirty, and it never touches the
//! [`SettingsStore`](crate::core::store) or the Apply/Reset chrome. So, like the
//! Sound page ([`super::sound`]), it renders directly from a GTK-free core model
//! ([`crate::core::network`]).
//!
//! # Render-from-status, delegate management
//!
//! On page entry (and on the "Refresh status" button) the page reads the active
//! connections via [`network::read_status`](crate::core::network::read_status) and
//! renders one row per connection — name, type, device — or a plain note when there
//! are none or NetworkManager cannot be read (R4.4-style degradation, never an
//! error). Only the status frame's *contents* are rebuilt on a refresh; the two
//! buttons are built once and stay put, so clicking "Refresh status" does not tear
//! down and recreate the very button being clicked (losing focus and churning
//! widgets for nothing). The single action is the "Open Network Settings" button,
//! which launches the management tool *detached* via
//! [`network::open_settings`](crate::core::network::open_settings); which tool that
//! is (nm-connection-editor, else kitty+nmtui) was decided from the detected
//! capabilities when the window built the page, and with neither available the
//! button is omitted entirely (R4.2).
//!
//! # The first read is deferred to first page entry (deliberate)
//!
//! [`build`] runs no `nmcli` at all — the status frame starts with a placeholder
//! and the first real read happens when the page first becomes the visible stack
//! child, through the same window hook that re-reads on every later entry. This
//! differs from the Sound page (which enumerates in `build`) for a startup-budget
//! reason: `build` runs inside the window's populate on the main thread, and a
//! wedged NetworkManager would hold a synchronous `nmcli` for the full 5 s command
//! timeout — stalling *every* category's appearance against the R8.1 cold-start
//! budget for a page the user is not even looking at. Deferring costs nothing: the
//! placeholder is visible at most for the instant before the entry hook fires.
//!
//! # Synchronous on the GTK main thread (deliberate)
//!
//! Once the page is actually viewed, the `nmcli` query is short-lived and the
//! launch is a fork-and-exit `setsid`, so — matching the Sound page's convention
//! (task 6.2) — both run synchronously on the GTK main thread through
//! [`CommandRunner::run`](crate::system::command::CommandRunner::run). There is no
//! staging pipeline to coordinate with, so no async machinery is warranted.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Frame, Label, Orientation, ScrolledWindow};

use crate::core::network::{self, ActiveConnection, Launcher, NetworkStatus};
use crate::system::command::SystemCommandRunner;

/// Outer margin, in pixels, around the page content.
const PAGE_MARGIN: i32 = 18;

/// Vertical spacing, in pixels, between sections and between rows.
const SECTION_SPACING: i32 = 12;

/// Horizontal spacing, in pixels, between a connection's name and its detail.
const ROW_SPACING: i32 = 8;

/// The mounted Network page: the scrollable root plus the handle the window uses to
/// re-read the status when the page is re-shown.
pub(crate) struct NetworkPage {
    /// The scrollable widget mounted in the window's stack.
    root: ScrolledWindow,
    /// The shared render state; this strong reference keeps the state — and thus the
    /// button handlers, which hold only [`std::rc::Weak`] references — alive for the
    /// life of the page.
    inner: Rc<Inner>,
}

impl NetworkPage {
    /// The widget to add to the window's stack.
    pub(crate) fn root(&self) -> &ScrolledWindow {
        &self.root
    }

    /// Re-reads the connection status and rebuilds the status rows — called by the
    /// window whenever the Network page becomes the visible stack child, which
    /// covers both the deferred *first* read (see the module docs) and picking up a
    /// connection change made while the app sat on another page (R3.1).
    pub(crate) fn refresh(&self) {
        self.inner.refresh();
    }
}

/// The shared render state the page's refresh handler operates on.
///
/// The handler captures a [`std::rc::Weak`] to this and upgrades on use, so the
/// widget tree (which owns the frame) never forms a reference cycle with the
/// closure mounted inside it — the same pattern as the Sound page.
struct Inner {
    /// The persistent "Active connections" frame whose *child* is replaced on each
    /// refresh. Only the status contents rebuild; the frame itself and the buttons
    /// around it are built once and never torn down (so a click on "Refresh
    /// status" does not destroy the button mid-handler).
    status_frame: Frame,
}

impl Inner {
    /// Re-reads the live connection status and re-renders the status frame's
    /// contents from it (R3.1). Nothing outside the frame is touched.
    fn refresh(&self) {
        let status = network::read_status(&SystemCommandRunner::new());
        self.status_frame
            .set_child(Some(&build_status_contents(&status)));
    }
}

/// Builds the Network page (task 6.9).
///
/// `launcher` is the window's capability-driven decision of which management tool
/// the "Open Network Settings" button spawns; `None` omits the button (R4.2).
/// Deliberately runs **no** `nmcli` here — the status frame starts with a
/// placeholder and the first read happens when the page first becomes visible (see
/// the module docs for the startup-budget rationale). The returned [`NetworkPage`]
/// must be kept alive by the window: it owns the strong reference to the render
/// state the refresh handler upgrades to. The window mounts [`NetworkPage::root`]
/// in the stack and calls [`NetworkPage::refresh`] whenever the page is shown.
pub(crate) fn build(launcher: Option<Launcher>) -> NetworkPage {
    let content = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    content.set_margin_top(PAGE_MARGIN);
    content.set_margin_bottom(PAGE_MARGIN);
    content.set_margin_start(PAGE_MARGIN);
    content.set_margin_end(PAGE_MARGIN);

    // The status frame's placeholder child; replaced by the first refresh, which
    // the window's page-entry hook triggers the moment the page becomes visible.
    let status_frame = Frame::new(Some("Active connections"));
    status_frame.set_child(Some(&note("Reading the network status…")));

    let inner = Rc::new(Inner {
        status_frame: status_frame.clone(),
    });

    // The static page skeleton (built once, never rebuilt — see the module docs):
    // the refresh button, the status frame, and the launcher button when a tool is
    // available.
    content.append(&build_refresh_button(&inner));
    content.append(&status_frame);
    if let Some(launcher) = launcher {
        content.append(&build_launcher_button(launcher));
    }

    let root = ScrolledWindow::new();
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.set_child(Some(&content));

    NetworkPage { root, inner }
}

/// The "Refresh status" button that re-reads the connections on demand — e.g.
/// after connecting through the spawned management tool while this page stays
/// visible (page re-entry also refreshes, but staying on the page would not).
///
/// Static: it re-renders only the status frame's contents, never itself.
fn build_refresh_button(inner: &Rc<Inner>) -> Button {
    let button = Button::with_label("Refresh status");
    button.set_halign(Align::Start);
    let weak = Rc::downgrade(inner);
    button.connect_clicked(move |_| {
        if let Some(inner) = weak.upgrade() {
            inner.refresh();
        }
    });
    button
}

/// Builds the status frame's contents: one row per active connection, or a plain
/// note when there are none or the status could not be read.
fn build_status_contents(status: &NetworkStatus) -> GtkBox {
    let section = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    section.set_margin_top(SECTION_SPACING);
    section.set_margin_bottom(SECTION_SPACING);
    section.set_margin_start(SECTION_SPACING);
    section.set_margin_end(SECTION_SPACING);

    match status {
        NetworkStatus::Connections(connections) if connections.is_empty() => {
            section.append(&note("No active connections."));
        }
        NetworkStatus::Connections(connections) => {
            for connection in connections {
                section.append(&connection_row(connection));
            }
        }
        NetworkStatus::Unavailable => {
            section.append(&note(
                "Could not read the network status from NetworkManager.",
            ));
        }
    }

    section
}

/// One connection's row: its name on the left, its type and device on the right.
fn connection_row(connection: &ActiveConnection) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, ROW_SPACING);

    let name = Label::new(Some(connection.name()));
    name.set_halign(Align::Start);
    name.set_hexpand(true);
    // Long SSID/profile names must not force the window wider; ellipsize instead.
    name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    name.set_xalign(0.0);
    row.append(&name);

    // A connection without a bound device (e.g. some VPNs) shows its type alone.
    let detail_text = if connection.device().is_empty() {
        connection.kind_label().to_string()
    } else {
        format!("{} ({})", connection.kind_label(), connection.device())
    };
    let detail = Label::new(Some(&detail_text));
    detail.set_halign(Align::End);
    row.append(&detail);

    row
}

/// The "Open Network Settings" button, launching the management tool detached
/// (task 6.9, R3.1). Only built when a tool is available.
fn build_launcher_button(launcher: Launcher) -> Button {
    let button = Button::with_label("Open Network Settings");
    button.set_halign(Align::Start);
    button.set_tooltip_text(Some(launcher.description()));
    button.connect_clicked(move |_| {
        network::open_settings(&SystemCommandRunner::new(), launcher);
    });
    button
}

/// A left-aligned, wrapping informational label for an empty/degraded state.
fn note(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.set_halign(Align::Start);
    label.set_wrap(true);
    label.set_xalign(0.0);
    label
}
