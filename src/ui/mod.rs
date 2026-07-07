//! The Relm4/GTK4 presentation layer (architecture §7).
//!
//! This is the only layer permitted to import `gtk`/`relm4`. It is
//! deliberately thin: widgets emit `SetValue` messages into the
//! [`crate::core`] store and re-render from store state, so no staging,
//! dirty-tracking, or conflict logic ever lives in a widget. The main window
//! (`GtkStackSidebar` + `GtkStack`) and the category pages are built here in
//! later tasks (§5 of `docs/tasks.md`).
