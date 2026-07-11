//! The main window shell: the category sidebar and content stack (task 5.1;
//! architecture §7; R2.1, R2.4, R4.2).
//!
//! # What this builds
//!
//! This module constructs the window's content: a horizontal [`GtkBox`](gtk4::Box)
//! holding a [`StackSidebar`](gtk4::StackSidebar) on the left and a
//! [`Stack`](gtk4::Stack) on the right. One stack page is added per *visible*
//! category (see [`super::category`]); the sidebar renders each page's title and
//! switches the stack when the user clicks it — the sidebar/stack layout required by
//! R2.4.
//!
//! A category's page is built one of two ways (see [`build_shell`]). When the
//! declarative row framework (task 5.2, [`super::page`]) has interim descriptors for
//! the category, its page is a rendered [`SettingsPage`](super::page::SettingsPage)
//! that round-trips edits through the store; per-row gating may also drop the whole
//! category when every row is hidden (R4.2). Otherwise the page is a task-5.1
//! placeholder — a scrollable titled container — that the per-category §6 content
//! replaces later. Every page is wrapped so the content area scrolls, keeping the
//! window usable at the small logical sizes R2.4 targets (a 1.33-scaled 2880×1800
//! panel and 1440p monitors) once pages fill with controls.
//!
//! # No libadwaita, no custom CSS (R2.1/R2.2)
//!
//! Only plain GTK4 widgets are used here — never libadwaita, which hard-codes the
//! Adwaita stylesheet and ignores `gtk-theme-name`, and never a `CssProvider` or any
//! other custom-CSS mechanism. The app renders with whatever system GTK theme is
//! active so it matches the rest of the desktop (R2.1). Layout is done purely with
//! widget properties (orientation, expansion, alignment, margins), which are not
//! styling. The rule is enforced durably by `tests/no_custom_css.rs`.

use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Label, Orientation, ScrolledWindow,
    Stack, StackSidebar,
};

use crate::core::detect::Capabilities;
use crate::ui::category::{SidebarCategory, visible_categories};
use crate::ui::page::{self, CategoryContent};

/// Title shown in the window's title bar and to the compositor.
const WINDOW_TITLE: &str = "Settings4000";

/// Default window width in logical pixels (R2.4).
///
/// Chosen to comfortably fit the sidebar plus a content pane on the smallest target
/// display — a 2880×1800 panel at 1.33 scaling is ~2165×1353 logical pixels, and
/// 1440p is 2560×1440 — while staying modest enough not to overflow either. It is a
/// starting size, not a floor: the content pane scrolls and expands, so the window
/// remains usable if the user shrinks it.
const DEFAULT_WIDTH: i32 = 960;

/// Default window height in logical pixels; see [`DEFAULT_WIDTH`].
const DEFAULT_HEIGHT: i32 = 640;

/// Margin, in pixels, around a page's content inside its scroller.
const CONTENT_MARGIN: i32 = 18;

/// Vertical spacing, in pixels, between controls stacked on a page. Placeholder
/// pages hold a single label today, but the spacing is set now so pages read
/// consistently once §6 adds rows.
const CONTENT_SPACING: i32 = 12;

/// Builds the top-level [`ApplicationWindow`] with its sidebar-plus-stack content,
/// showing one page per category visible under `capabilities` (R4.2).
///
/// This is what task 1.3's bootstrap now presents in place of the previously empty
/// window. `capabilities` is detected once at startup by the caller (`super::app`);
/// a category whose gate is not satisfied is simply never added (see
/// [`build_shell`]).
pub(crate) fn build(app: &Application, capabilities: &Capabilities) -> ApplicationWindow {
    let window = ApplicationWindow::builder()
        .application(app)
        .title(WINDOW_TITLE)
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .build();

    window.set_child(Some(&build_shell(capabilities)));
    window
}

/// Builds the sidebar-plus-stack content box for the visible categories (R2.4,
/// R4.2).
///
/// The stack expands to fill the space beside the sidebar; the sidebar takes its
/// natural width. Only categories returned by [`visible_categories`] (task 5.1's
/// coarse gate) are considered, and each is then resolved by [`page::build_category`]
/// (task 5.2's per-row gate). This composes the two gates (R4.2):
///
/// - [`CategoryContent::Framework`] — the category has visible framework rows, so its
///   rendered page is mounted.
/// - [`CategoryContent::Emptied`] — the category has framework rows but every one is
///   gated out, so it is dropped entirely (the "hide the whole category when
///   emptied" half of R4.2 the coarse gate cannot see); [`page::build_category`] has
///   already logged it.
/// - [`CategoryContent::NoSpec`] — the category has no framework rows yet, so its
///   task-5.1 placeholder is shown until §6 fills it in.
///
/// If that leaves the stack empty the sidebar is simply empty, which is the correct
/// degenerate result rather than a crash.
fn build_shell(capabilities: &Capabilities) -> GtkBox {
    let stack = Stack::new();
    // The content pane fills the width and height left beside the sidebar so pages
    // (and their scrollers) use the whole window (R2.4).
    stack.set_hexpand(true);
    stack.set_vexpand(true);

    // A GtkStackSidebar renders the titles of whichever stack it is bound to and
    // switches the visible page on click — the sidebar navigation of R2.4.
    let sidebar = StackSidebar::new();
    sidebar.set_stack(&stack);

    for category in visible_categories(capabilities) {
        // `add_titled` registers a page under its machine name and the title the
        // sidebar displays (see `SidebarCategory::stack_name`/`title`).
        match page::build_category(category, capabilities) {
            CategoryContent::Framework(widget) => {
                stack.add_titled(&widget, Some(category.stack_name()), category.title());
            }
            CategoryContent::NoSpec => {
                let placeholder = build_placeholder_page(category);
                stack.add_titled(&placeholder, Some(category.stack_name()), category.title());
            }
            // Every row gated out: drop the category (R4.2). Already logged by
            // `build_category`.
            CategoryContent::Emptied => {}
        }
    }

    let shell = GtkBox::new(Orientation::Horizontal, 0);
    shell.append(&sidebar);
    shell.append(&stack);
    shell
}

/// Builds the placeholder content for one category page (task 5.1).
///
/// The page is a [`ScrolledWindow`] wrapping a vertical box — the shell the real
/// controls plug into in later tasks (5.2/§6). The scroller is established now so
/// the content area already scrolls when a page grows taller than the window,
/// keeping it usable at the small logical sizes R2.4 targets. For now the box holds
/// a single label with the category title so each page renders as distinct, non-empty
/// content while the shell is verified.
fn build_placeholder_page(category: SidebarCategory) -> ScrolledWindow {
    let content = GtkBox::new(Orientation::Vertical, CONTENT_SPACING);
    content.set_margin_top(CONTENT_MARGIN);
    content.set_margin_bottom(CONTENT_MARGIN);
    content.set_margin_start(CONTENT_MARGIN);
    content.set_margin_end(CONTENT_MARGIN);

    // Left-aligned so the heading (and the rows §6 adds beneath it) start at the
    // page's leading edge rather than centring.
    let heading = Label::new(Some(category.title()));
    heading.set_halign(Align::Start);
    content.append(&heading);

    let scroller = ScrolledWindow::new();
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&content));
    scroller
}
