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
//!
//! The real per-category page content (§6) plugs into this shell in later tasks.

pub(crate) mod app;
mod category;
mod chrome;
mod display;
mod page;
mod row;
mod sound;
mod startup;
mod window;
