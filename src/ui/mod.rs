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
//! - [`window`] — the main window shell built at activation: a `GtkStackSidebar`
//!   plus a `GtkStack` with one page per visible category (task 5.1).
//! - [`category`] — the seven sidebar categories and the pure, headlessly tested
//!   rule that decides which of them to show for the detected capabilities (R4.2).
//! - [`row`] — the GTK-free declarative row framework: descriptors, widget kinds,
//!   per-row capability gating, and the value conversions the controls use (task
//!   5.2, R2.3). Kept headlessly testable (R6.2).
//! - [`page`] — the Relm4 page component that renders a descriptor list into live
//!   controls and runs the `SetValue` → store → render loop (task 5.2).
//!
//! The real per-category page content (§6) and shared-store startup wiring (task
//! 5.4) plug into this shell in later tasks.

pub(crate) mod app;
mod category;
mod page;
mod row;
mod window;
