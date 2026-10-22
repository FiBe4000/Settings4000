//! The Relm4/GTK4 presentation layer (architecture §7).
//!
//! This is the only layer permitted to import `gtk`/`relm4`. It is
//! deliberately thin: widgets emit `SetValue` messages into the
//! [`crate::core`] store and re-render from store state, so no staging,
//! dirty-tracking, or conflict logic ever lives in a widget.
//!
//! The layer is made up of:
//!
//! - [`app`] — the process bootstrap: the `gtk4::Application`, its single-instance
//!   registration (R8.4), and window activation.
//! - [`about`] — the header bar's application menu and the About window it opens (task
//!   10.2, R2.5), whose content is a pure value built from the crate's package metadata.
//! - [`window`] — the main window: the `GtkStackSidebar` + `GtkStack` of category
//!   pages, and the Apply/Reset chrome, dirty markers, toast, and detection refresh
//!   around them, driven by one shared store (tasks 5.1/5.3).
//! - [`category`] — the seven sidebar categories and the pure, headlessly tested
//!   rule that decides which of them to show for the detected capabilities (R4.2).
//! - [`row`] — the GTK-free declarative row framework: descriptors, widget kinds,
//!   per-row capability gating, and the value conversions the controls use (task
//!   5.2, R2.3). Kept headlessly testable (R6.2).
//! - [`page`] — the Relm4 page component that renders a descriptor list into live
//!   controls and runs the `SetValue` → store → render loop (task 5.2).
//! - [`chrome`] — the Apply/Reset chrome: the pure decisions (dirty→enabled/marker,
//!   apply-outcome→toast/dialog/commit, refresh-report→conflict warning) plus the
//!   plain-GTK toast and warning dialog (task 5.3). The decisions are headlessly
//!   tested (R6.2).
//! - [`startup`] — the worker-thread glue of the startup load (tasks 5.4, 9.21): it runs
//!   the GTK-free [`crate::core::load`] pass (detection plus parsing every backing
//!   config file) on a worker thread and hands the result back to the main thread,
//!   where the window applies it.
//! - [`worker`] — the one hop off the GTK main thread and back (tasks 5.4, 10.3): blocking
//!   work (the startup load, the laptop-display toggle script) runs on a worker thread
//!   and only its result returns to the main loop, so the window never freezes.
//! - [`display`] — the Display page's bespoke glue (task 6.1): per-monitor
//!   resolution/refresh/scale/position drop-downs and a per-monitor enable switch
//!   rendered from the live [`crate::core::display`] model, plus the
//!   runtime-only laptop-display toggle. It is bespoke rather than declarative because
//!   monitors are dynamic and the laptop toggle bypasses staging (R5.2).
//! - [`sound`] — the Sound page's bespoke glue (task 6.2): output/input device
//!   drop-downs, volume sliders, and mute switches rendered from the runtime-loaded
//!   [`crate::core::sound`] state. It is bespoke rather than declarative because every
//!   control is runtime-only — it applies a `wpctl` command immediately and bypasses
//!   staging entirely (R3.1/R5.2).
//! - [`theme`] — the Theme page's bespoke glue (task 6.3): a multi-section page whose
//!   first section is the palette-scheme drop-down (plus a color preview strip),
//!   rendered from the [`crate::core::theme`] model. It is bespoke rather than
//!   declarative because a palette switch runs `generate-colors` instead of writing a
//!   store-backed setting; tasks 6.4/6.5 add further sections.
//! - [`input`] — the Input page's descriptor glue (task 6.6): the row list for the
//!   store-backed keyboard/mouse/touchpad settings, whose keyboard-layout add-control is
//!   populated from the runtime-loaded XKB registry. It uses the declarative row
//!   framework (the settings are ordinary file-backed [`crate::core::model::SettingId`]s);
//!   only the runtime candidate list and the curated keyboard-option switches need this
//!   thin glue. The store→`input.lua` write itself lives in [`crate::core::input`].
//! - [`network`] — the Network page's bespoke glue (task 6.9): a read-only list of the
//!   active NetworkManager connections plus the "Open Network Settings" button that
//!   launches the management tool detached, rendered from the runtime
//!   [`crate::core::network`] status. It is bespoke rather than declarative because the
//!   page is read-only and runtime-backed (R3.1) — it has no store-backed settings at
//!   all.
//! - [`notifications`] — the Notifications page's bespoke glue (tasks 11.7, 11.15): the
//!   runtime-only notifd controls (do-not-disturb, the notification count and inhibited
//!   state, open the center, clear all behind a confirmation), rendered from the
//!   GTK-free [`crate::core::notifications`] model, plus the row list of notifd's staged
//!   settings, which the window renders through the declarative framework and mounts
//!   inside the page. Bespoke because its runtime half is not staged (R5.2).
//! - [`widgets`] — the small widget builders (labelled row, informational note,
//!   drop-down) and the margin/spacing metrics every page above shares, kept as one
//!   copy so the pages cannot drift apart visually (task 9.19).
//!
//! Every category page above is mounted into the window shell, which builds each one
//! from its category's builder — a bespoke page, the declarative framework, or both.
//!
//! # Naming conventions
//!
//! The UI uses the vocabulary of [`crate::core`]'s naming conventions (dirty, scan,
//! detect, load, refresh) plus four verbs for bringing widgets up to date, each with
//! one meaning:
//!
//! - **render** — `render(value)` sets one bound control to a given value with its
//!   change signal blocked, so the programmatic set is never taken for a user edit
//!   (`ui::page`). It is the per-control step every repaint of a framework page uses.
//! - **rerender** — repaint a page from the state it already holds (the store or its
//!   model) after something outside the page changed that state: a Reset, a committed
//!   Apply, a conflict refresh. It never queries a source. The window's
//!   `MountedPages::rerender_*` helpers, `PageMsg::Rerender` and each bespoke page's
//!   `rerender` follow it.
//! - **rebuild** — tear down a container's children and build new widgets from state,
//!   for content whose *set* of widgets depends on that state (one section per monitor
//!   or audio device, one row per list item). A page may implement its `rerender` as a
//!   rebuild.
//! - **refresh** — query the source again (a model's `load`, `reload` or `refresh`),
//!   then rerender or rebuild from the result. The runtime pages' page-entry hook and
//!   in-page refresh buttons, the window's Refresh button (R4.3) and the conflict path
//!   are all refreshes. A user-visible button label may still say "Rescan devices" or
//!   "Check again"; the code behind it is a `refresh`.

mod about;
pub mod app;
mod category;
mod chrome;
mod display;
mod input;
mod network;
// Crate-visible (not `pub`) so the test-support module (`crate::testing`, task 7.2) can
// re-expose its GTK-free staged-row list, letting the Notifications integration suite
// check which rows the page shows (task 11.15). Nothing outside the crate can reach it
// directly; the public surface stays `ui::app`.
pub(crate) mod notifications;
mod page;
mod row;
mod sound;
mod startup;
mod theme;
mod widgets;
mod window;
mod worker;
