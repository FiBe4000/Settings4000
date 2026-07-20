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
//! - [`startup`] — the worker-thread startup load (task 5.4): the GTK-free logic that
//!   runs detection and parses the backing config files off the main thread, which
//!   the window applies on completion. Headlessly tested (R6.2).
//! - [`display`] — the Display page's bespoke glue (task 6.1): per-monitor
//!   resolution/refresh/scale/position drop-downs and a per-monitor enable switch
//!   rendered from the runtime-discovered [`crate::core::display`] model, plus the
//!   runtime-only laptop-display toggle. It is bespoke rather than declarative because
//!   monitors are dynamic and the laptop toggle bypasses staging (R5.2).
//! - [`sound`] — the Sound page's bespoke glue (task 6.2): output/input device
//!   drop-downs, volume sliders, and mute switches rendered from the runtime-enumerated
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
//!   thin glue. The store→`input.conf` write itself lives in [`crate::core::input`].
//! - [`network`] — the Network page's bespoke glue (task 6.9): a read-only list of the
//!   active NetworkManager connections plus the "Open Network Settings" button that
//!   launches the management tool detached, rendered from the runtime
//!   [`crate::core::network`] status. It is bespoke rather than declarative because the
//!   page is read-only and runtime-backed (R3.1) — it has no store-backed settings at
//!   all.
//! - [`notifications`] — the Notifications page's bespoke Do-Not-Disturb glue (task 6.7):
//!   the position/timeout controls are ordinary store-backed framework rows (built via
//!   [`page`]), so this module supplies only the runtime-only DND switch, which reads and
//!   sets live daemon state via `swaync-client` and bypasses staging (R5.2). The
//!   store→`config.json` write for position/timeout lives in
//!   [`crate::core::notifications`].
//!
//! The real per-category page content (§6) plugs into this shell in later tasks.

pub mod app;
mod category;
mod chrome;
mod display;
mod input;
mod network;
mod notifications;
mod page;
mod row;
mod sound;
// `startup` and `window` are crate-visible (not `pub`) so the test-support
// module (`crate::testing`, task 7.2) can re-expose their GTK-free pieces —
// the backing-file loaders and the base Apply-plan builder — letting the
// integration suites drive the exact code paths the app runs. Nothing outside
// the crate can reach them directly; the public surface stays `ui::app`.
pub(crate) mod startup;
mod theme;
pub(crate) mod window;
