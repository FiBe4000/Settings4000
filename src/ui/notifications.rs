//! The Notifications page's bespoke Do-Not-Disturb glue (task 6.7; architecture §7;
//! R4.2, R5.2).
//!
//! # Why only the DND switch is here
//!
//! The Notifications page is a hybrid. Its **position** and **auto-dismiss timeout** are
//! ordinary file-backed settings, so they are built by the declarative row framework
//! (task 5.2) from the descriptors in [`super::page`] and staged in the shared store — the
//! window mounts that framework page directly. This module supplies the *one* control the
//! framework cannot: the **Do-Not-Disturb switch**, which is **runtime-only** (R5.2).
//!
//! swaync's DND is live daemon state, not a persisted `config.json` key (see
//! [`crate::core::notifications`]), so it is applied *immediately* via `swaync-client`,
//! exactly like the Sound page's volume/mute controls (task 6.2): it reads its initial
//! state from the running daemon and writes the new state at once, bypassing staging
//! entirely. It therefore cannot be a store-backed row, so the window appends this bespoke
//! switch beside the framework rows.
//!
//! # Synchronous on the GTK main thread (deliberate)
//!
//! The `swaync-client` calls are short-lived, so — matching the Sound page's convention
//! (task 6.2) — they run synchronously on the GTK main thread through
//! [`CommandRunner::run`](crate::system::command::CommandRunner::run) rather than a worker.
//! There is no staging pipeline to coordinate with, so no async machinery is warranted.
//!
//! # Render-from-daemon, apply-immediately
//!
//! [`build_dnd_section`] queries the live DND state and reflects it on the switch before
//! connecting the change handler, so the initial programmatic set is never mistaken for a
//! user toggle. Flipping the switch runs `swaync-client --dnd-on`/`--dnd-off` at once. The
//! window re-queries the daemon whenever the page is re-shown (via [`Self::refresh_dnd`]),
//! so a DND change made elsewhere — e.g. a keybind — is reflected on page entry; that
//! programmatic set blocks the change handler so it, too, is not seen as a user edit.

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Label, Orientation, Switch, glib};

use crate::core::notifications;
use crate::system::command::SystemCommandRunner;

/// Horizontal spacing, in pixels, between the DND label and its switch — matched to the
/// framework page's row spacing so the DND row lines up with the position/timeout rows it
/// is appended beneath.
const ROW_SPACING: i32 = 8;

/// The Notifications page's bespoke Do-Not-Disturb control, retained by the window.
///
/// It owns the switch and its change-handler id so the window can re-query the daemon and
/// re-render the switch on page entry without the render being mistaken for a user toggle.
/// The position/timeout controls are *not* here — they are the framework page's business.
pub(crate) struct NotificationsPage {
    /// The row widget (label + switch) the window appends to the framework page.
    section: GtkBox,
    /// The do-not-disturb switch, re-rendered by [`Self::refresh_dnd`].
    dnd_switch: Switch,
    /// The switch's `notify::active` handler, blocked while re-rendering so a
    /// programmatic set never fires an apply (the same discipline the framework and Sound
    /// controls use).
    dnd_handler: glib::SignalHandlerId,
}

impl NotificationsPage {
    /// The widget the window appends beside the framework rows.
    pub(crate) fn widget(&self) -> &GtkBox {
        &self.section
    }

    /// Re-queries the daemon's do-not-disturb state and updates the switch (task 6.7,
    /// R5.2).
    ///
    /// Called by the window when the Notifications page becomes visible, so a DND change
    /// made elsewhere (e.g. a compositor keybind) is reflected on page entry. The change
    /// handler is blocked around the set so this render does not issue a redundant
    /// `swaync-client` command. An unreachable daemon degrades to "off".
    pub(crate) fn refresh_dnd(&self) {
        let active = notifications::dnd_state(&SystemCommandRunner::new()).unwrap_or(false);
        self.dnd_switch.block_signal(&self.dnd_handler);
        self.dnd_switch.set_active(active);
        self.dnd_switch.unblock_signal(&self.dnd_handler);
    }
}

/// Builds the runtime-only Do-Not-Disturb row, reading its initial state from the running
/// swaync daemon (task 6.7, R5.2).
///
/// The returned [`NotificationsPage`] must be kept alive by the window: its handler drives
/// the immediate `swaync-client` apply, and the window re-queries it on page entry via
/// [`NotificationsPage::refresh_dnd`]. The switch is set to the live state *before* the
/// change handler is connected, so the initial render is not mistaken for a user toggle.
pub(crate) fn build_dnd_section() -> NotificationsPage {
    let dnd_switch = Switch::new();
    dnd_switch.set_halign(Align::End);
    dnd_switch.set_valign(Align::Center);
    // Unlike the staged position/timeout controls, this one applies the moment it is
    // toggled — spell that out so the mixed staged/immediate page does not surprise.
    dnd_switch.set_tooltip_text(Some("Applied immediately (not staged)"));

    // Reflect the live daemon state before wiring the handler (an unreachable daemon
    // shows as off).
    let active = notifications::dnd_state(&SystemCommandRunner::new()).unwrap_or(false);
    dnd_switch.set_active(active);

    let dnd_handler = dnd_switch.connect_active_notify(|switch| {
        // Runtime-only (R5.2): apply the new state to the daemon immediately; nothing is
        // staged or written to config.json.
        notifications::set_dnd(&SystemCommandRunner::new(), switch.is_active());
    });

    let section = GtkBox::new(Orientation::Horizontal, ROW_SPACING);
    let label = Label::new(Some("Do Not Disturb"));
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    section.append(&label);
    section.append(&dnd_switch);

    NotificationsPage {
        section,
        dnd_switch,
        dnd_handler,
    }
}
