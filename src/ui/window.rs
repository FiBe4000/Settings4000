//! The main window: the category sidebar and content stack (task 5.1), the
//! Apply/Reset chrome, dirty markers, toast, and detection refresh (task 5.3), and
//! the worker-thread startup sequencing that populates them (task 5.4; architecture
//! §7, §8; R2.1, R2.4, R4.2, R4.3, R5.1, R5.5, R5.6, R8.1).
//!
//! # What this builds
//!
//! The window has a [`HeaderBar`] titlebar carrying the chrome — a **Refresh** button,
//! a **Reset** button, a suggested-action **Apply** button, and the application menu
//! with its About entry (task 10.2, R2.5) — over a content area
//! that is a [`StackSidebar`] beside a [`Stack`] of category pages, all wrapped in an
//! [`Overlay`] so a non-fatal [`Toast`](super::chrome::Toast) can float over it (R5.5).
//! One stack page is added per *visible* category (see [`super::category`]); a
//! [`super::page::PagePlan`] decides whether each is a rendered
//! [`SettingsPage`](super::page::SettingsPage), a task-5.1 placeholder, or dropped
//! entirely when its rows are all gated out (R4.2).
//!
//! # The persistent shell vs. the repopulated pages (task 5.1/5.4)
//!
//! Everything structural — the window, the titlebar with its chrome buttons, the
//! overlay, the sidebar, and the (initially empty) stack — is built **once**, up front,
//! and never rebuilt. Only the stack's *category pages* are (re)populated: on startup
//! when the worker delivers detection results, and again on each manual refresh. This
//! split matters for a concrete GTK reason as much as a conceptual one: GTK only
//! permits [`ApplicationWindow::set_titlebar`] on an *unrealized* window, and the shell
//! is populated after the window has been presented (the worker runs concurrently), so
//! the titlebar must be set once before that — swapping it later would warn and may not
//! take effect. [`Shell`] bundles the shared handles the population and chrome closures
//! operate on; [`Shell::populate`] is the single routine that (re)builds the pages.
//!
//! # Startup sequencing: build the shell now, populate when the worker completes (5.4)
//!
//! Cold start must stay under the R8.1 budget, so the window does **not** block on
//! detection or config parsing (architecture §8). [`build`] returns a fully assembled
//! shell whose stack shows only a lightweight loading placeholder, having handed the
//! slow work — detection (task 4.3) and parsing every backing config file (§3 parsers)
//! — to a worker thread via [`spawn_load`], which runs [`crate::core::load::load`]. The
//! parsed [`StartupLoad`] is delivered back to the main thread over a channel and
//! applied by [`Shell::apply_startup_load`]: it populates the shared store from the
//! real config files (establishing the true `original` values and freshness
//! baselines) and rebuilds the stack pages from the detected [`Capabilities`]. A
//! missing binary/daemon, an unreadable/unparseable config, or an absent repo never
//! blocks or crashes startup — detection degrades to absent and the loader skips the
//! file (R4.3/R4.4) — so the window always comes up with whatever is available.
//!
//! # One shared store drives the chrome (task 5.3)
//!
//! The window owns a **single** [`SettingsStore`] behind an `Rc<RefCell<…>>` that every
//! page shares. This is what makes the chrome meaningful: an edit on any page stages
//! into the one store, so the Apply/Reset buttons (enabled while
//! [`SettingsStore::is_dirty`]) and the per-page `needs-attention` markers (from
//! [`SettingsStore::is_category_dirty`]) all read the same dirty state. Each page
//! reports an edit through a shared `on_changed` callback ([`Shell::update_chrome`]) so
//! the window re-derives the chrome immediately; the window drives the pages back by
//! sending each retained [`Controller`] a [`PageMsg::Rerender`] after a Reset, commit,
//! or conflict reload.
//!
//! # Apply wires the real pipeline and the store's freshness tracker (task 4.5 / 5.4)
//!
//! **Apply** runs [`apply::run`] and dispatches its [`ApplyOutcome`] through the pure
//! [`chrome::respond_to_apply`]: an [`ApplyOutcome::Applied`] is committed to the store
//! (via [`SettingsStore::commit_apply`], which promotes staged→original and re-baselines
//! the written files so the app's own write is not seen as a conflict next time),
//! optionally raising a toast for non-fatal reload failures (R5.5); every other outcome
//! (invalid values, an external conflict, a write failure) is surfaced as a modal
//! warning without committing. The pipeline is given the **store's** freshness tracker
//! (via [`SettingsStore::freshness`]), so its step-2 conflict check measures the target
//! files against the real baselines the startup load recorded (R5.6) rather than an
//! empty tracker.
//!
//! Building that plan out of the store and the bespoke page models — which writes to
//! fold in, in which order, and when to refuse to apply at all — is **not** done here:
//! it is [`assemble_apply_plan`](crate::core::assemble::assemble_apply_plan) in the
//! GTK-free [`core::assemble`](crate::core::assemble) module, so those decisions are
//! testable without a display (R6.2; task 9.16). What stays with the window is the part
//! that needs widgets or side effects: running the pipeline, refreshing a model whose
//! files changed on disk ([`Shell::refresh_conflicted_source`]), committing the sources
//! the assembler reports as having contributed ([`Shell::commit_applied`]), and showing
//! the dialog or toast.
//!
//! # No libadwaita, no custom CSS (R2.1/R2.2)
//!
//! Only plain GTK4 widgets are used — never libadwaita, and never a `CssProvider` or
//! any custom-CSS mechanism. The Apply button's `suggested-action` and the toast's
//! `osd` are *built-in* theme style classes applied with `add_css_class`, which selects
//! styling the active theme already provides rather than shipping our own; the rule is
//! enforced by `tests/no_custom_css.rs`.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, HeaderBar, Label, MenuButton,
    Orientation, Overlay, ScrolledWindow, Spinner, Stack, StackSidebar, Widget, glib,
};
use relm4::{ComponentController, Controller};

use crate::core::apply;
use crate::core::assemble::{
    ApplySources, ConflictedSource, ModelCommits, PrepareFailure, assemble_apply_plan,
};
use crate::core::detect::Capabilities;
use crate::core::display::DisplayModel;
use crate::core::hyprlang_notes::SettingNote;
use crate::core::input::InputModel;
use crate::core::load::{LoadedFile, StartupLoad, adopt_refreshed_model, merge_refreshed_files};
use crate::core::model::Category;
use crate::core::notifd_config::NotifdConfigModel;
use crate::core::power::PowerModel;
use crate::core::store::SettingsStore;
use crate::core::theme::{PaletteModel, ThemesModel, WallpaperModel};
use crate::system::command::{CommandRunner, SystemCommandRunner};
use crate::system::signal::SystemProcessSignaller;
use crate::ui::about::{self, AboutInfo};
use crate::ui::category::{SidebarCategory, visible_categories};
use crate::ui::chrome::{self, ApplyResponse, Toast};
use crate::ui::display::{self, DisplayPage};
use crate::ui::input;
use crate::ui::network::{self, NetworkPage};
use crate::ui::notifications::{self, NotificationsPage};
use crate::ui::page::{self, PageMsg, PagePlan, SettingsPage};
use crate::ui::sound::{self, SoundPage};
use crate::ui::startup::spawn_load;
use crate::ui::theme::{self, ThemePage};
use crate::ui::widgets::{PAGE_MARGIN, SECTION_SPACING};

/// Title shown in the window's title bar and to the compositor, and the application
/// name the About window displays (task 10.2), so the two cannot spell it differently.
pub(super) const WINDOW_TITLE: &str = "Settings4000";

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

/// The `GtkStack` name of the loading placeholder page shown while the worker runs.
///
/// It is added with a name but no title, so [`StackSidebar`] does not show a row for
/// it — the sidebar is empty during loading — and [`Shell::populate`] removes it once
/// the real pages arrive.
const LOADING_PAGE_NAME: &str = "__loading";

/// The bespoke per-category models the window owns, shared with the page glue and the
/// Apply/Reset/Refresh wiring (tasks 6.1, 6.3–6.8, 11.4, 11.15).
///
/// Every field starts `None` and is filled by the startup worker ([`spawn_load`]) — and
/// replaced or kept on each manual refresh, see [`Shell::apply_refresh_load`] — or stays
/// `None` when the machine cannot back it (the reason is logged by `core::load::load`);
/// the page that renders from it then hides the affected section, page, or category
/// (R4.2).
///
/// The models come in two kinds, and the difference decides how the window treats them:
///
/// - **Staging sources** (`display`, `palette`, `themes`, `wallpaper`) hold staged edits
///   of their own, beside the shared [`SettingsStore`], because their values are not
///   store settings (monitors are known only at runtime; a palette switch runs
///   `generate-colors`; the theme and wallpaper changes are multi-file writes). Each one
///   feeds its dirty state into the same Apply/Reset chrome ([`Self::display_dirty`],
///   [`Self::theme_dirty`]), is cleared by Reset ([`Self::reset_staged`]), is committed
///   on its own after an Apply ([`Shell::commit_applied`]), and is kept wholesale on a
///   refresh while it holds staged edits *and the refresh still produced a replacement*
///   (a dirty model whose app or source went away is dropped; see
///   [`adopt_refreshed_model`]).
/// - **Write helpers** (`input`, `notifd_config`, `power`) are *not* separate staging
///   sources: every setting of their page is staged in the shared store as an ordinary
///   framework row, so they feed neither the dirty rollup nor a bespoke commit and are
///   simply replaced on a refresh. They only supply what the store cannot — row
///   candidates, and the rendering of the store's dirty settings into their file's write
///   on Apply.
///
/// Every staging source and write helper is folded into the one Apply plan by
/// [`assemble_apply_plan`]. The struct is [`Clone`] for the same reason as [`Shell`]: each
/// field is an `Rc`, so a clone shares the models.
#[derive(Clone, Default)]
struct PageModels {
    /// The Display page's live monitors (task 6.1); a staging source whose
    /// `monitors.conf` write joins the Apply plan. `None` without a live compositor.
    display: Rc<RefCell<Option<DisplayModel>>>,
    /// The Theme page's palette scheme (task 6.3); a staging source whose switch joins the
    /// Apply plan as a `PaletteSwitch`. `None` without a dotfiles palette source.
    palette: Rc<RefCell<Option<PaletteModel>>>,
    /// The Theme page's GTK/icon/cursor themes (task 6.4); a staging source with
    /// multi-file theme/cursor writes. `None` when `gsettings` is absent.
    themes: Rc<RefCell<Option<ThemesModel>>>,
    /// The Theme page's wallpaper and lock background (task 6.5); a staging source writing
    /// `hyprpaper.conf` / `hyprlock.conf`. `None` when hyprpaper is absent.
    wallpaper: Rc<RefCell<Option<WallpaperModel>>>,
    /// The Input page's write helper (tasks 6.6, 11.4): the keyboard-layout candidates
    /// and the `input.lua` write. `None` — and the Input page hidden — without `hyprctl`,
    /// on a hyprlang host, or when `input.lua` is missing or not fully understood (see
    /// [`load_input_from`](crate::core::load::load_input_from)).
    input: Rc<RefCell<Option<InputModel>>>,
    /// The Notifications page's write helper for the staged notifd settings (task
    /// 11.15), whose file write the pipeline follows with notifd's `Reload()`. `None`
    /// when notifd is not running at version 1.1 or later, or its file is missing or not
    /// understood (see
    /// [`load_notifd_config_from`](crate::core::load::load_notifd_config_from)); the page
    /// then shows only its runtime controls.
    notifd_config: Rc<RefCell<Option<NotifdConfigModel>>>,
    /// The Power & Idle page's write helper (task 6.8): the `hypridle.conf` write for the
    /// store-backed timeouts and lock command, followed by a hypridle restart. `None` when
    /// hypridle is absent.
    power: Rc<RefCell<Option<PowerModel>>>,
}

impl PageModels {
    /// Whether the Display model holds staged monitor edits (`false` without one).
    fn display_dirty(&self) -> bool {
        self.display
            .borrow()
            .as_ref()
            .is_some_and(DisplayModel::is_dirty)
    }

    /// Whether any of the Theme page's three staging models holds staged edits.
    fn theme_dirty(&self) -> bool {
        let palette_dirty = self
            .palette
            .borrow()
            .as_ref()
            .is_some_and(PaletteModel::is_dirty);
        let themes_dirty = self
            .themes
            .borrow()
            .as_ref()
            .is_some_and(ThemesModel::is_dirty);
        let wallpaper_dirty = self
            .wallpaper
            .borrow()
            .as_ref()
            .is_some_and(WallpaperModel::is_dirty);
        palette_dirty || themes_dirty || wallpaper_dirty
    }

    /// Discards the staged edits of every staging source (R5.1). The write helpers hold
    /// none — their settings are reset with the store.
    fn reset_staged(&self) {
        if let Some(display) = self.display.borrow_mut().as_mut() {
            display.reset();
        }
        if let Some(palette) = self.palette.borrow_mut().as_mut() {
            palette.reset();
        }
        if let Some(themes) = self.themes.borrow_mut().as_mut() {
            themes.reset();
        }
        if let Some(wallpaper) = self.wallpaper.borrow_mut().as_mut() {
            wallpaper.reset();
        }
    }
}

/// The category pages currently mounted in the window's stack, retained so the window
/// can drive them after the fact: re-render them after a Reset, a committed Apply, or a
/// conflict reload; probe the runtime pages on page entry ([`wire_page_entry`]); and
/// keep the per-page dirty markers in step with the staged state.
///
/// Everything here is rebuilt by [`Shell::populate`] — categories, sections and rows may
/// appear or disappear between populates — and a bespoke-page handle is `None` while
/// its category is hidden or shows a placeholder instead. Only the Display, Theme and
/// framework pages stage anything; the Sound, Network and runtime Notifications pages
/// are runtime-only or read-only, so they never feed the Apply/Reset chrome or a dirty
/// marker (R3.1, R5.2) and are retained only for their page-entry probe. [`Clone`]
/// shares the handles, as for [`Shell`].
#[derive(Clone, Default)]
struct MountedPages {
    /// The bespoke Display page (task 6.1), re-rendered from [`PageModels::display`].
    /// `None` while the category shows a placeholder (no live monitor data).
    display: Rc<RefCell<Option<DisplayPage>>>,
    /// The bespoke Theme page (tasks 6.3–6.5), re-rendered from the three Theme models.
    /// `None` while the category shows a placeholder (no Theme model at all).
    theme: Rc<RefCell<Option<ThemePage>>>,
    /// The runtime-only Sound page (task 6.2), probed on every page entry — including
    /// its deferred first load (task 9.4).
    sound: Rc<RefCell<Option<SoundPage>>>,
    /// The Notifications page (task 11.7), whose notifd daemon state is read on every
    /// page entry (the first read included, task 9.26). Its staged notifd rows are a
    /// framework page whose controller sits in `controllers` like any other.
    notifications: Rc<RefCell<Option<NotificationsPage>>>,
    /// The read-only Network page (task 6.9), whose connection status is re-read on
    /// every page entry.
    network: Rc<RefCell<Option<NetworkPage>>>,
    /// The framework pages' controllers, so the window can send them
    /// [`PageMsg::Rerender`] and so their widgets stay alive while mounted.
    controllers: Rc<RefCell<Vec<Controller<SettingsPage>>>>,
    /// The `(Category, stack page)` pairs whose `needs-attention` marker tracks per-page
    /// dirty state; read by [`Shell::update_chrome`].
    marked: Rc<RefCell<Vec<(Category, Widget)>>>,
}

impl MountedPages {
    /// Drops the retained bespoke-page handles ahead of a repopulate; see
    /// [`Shell::populate`] for why this must happen before the stack is torn down.
    fn release_bespoke(&self) {
        *self.display.borrow_mut() = None;
        *self.sound.borrow_mut() = None;
        *self.notifications.borrow_mut() = None;
        *self.theme.borrow_mut() = None;
        *self.network.borrow_mut() = None;
    }

    /// Records `root` as the stack page whose dirty marker follows `category`.
    fn mark(&self, category: Category, root: Widget) {
        self.marked.borrow_mut().push((category, root));
    }

    /// Re-renders every page that stages something, after the window changed the staged
    /// state from outside a page (a Reset or a committed Apply), so a control that no
    /// longer matches its store or model snaps back.
    fn rerender_all(&self) {
        self.rerender_framework();
        self.rerender_display();
        self.rerender_theme();
    }

    /// Sends every retained framework-page controller a [`PageMsg::Rerender`] so its
    /// controls re-render from the shared store (task 5.3).
    fn rerender_framework(&self) {
        for controller in self.controllers.borrow().iter() {
            // The receiver only goes away if the page's runtime has stopped, which cannot
            // happen while the window holds its controller; ignore the send result rather
            // than unwrap a case that cannot occur here.
            let _ = controller.sender().send(PageMsg::Rerender);
        }
    }

    /// Re-renders the Display page from its model (task 6.1), so its drop-downs snap to
    /// the model's values.
    fn rerender_display(&self) {
        if let Some(page) = self.display.borrow().as_ref() {
            page.rerender();
        }
    }

    /// Re-renders the Theme page from its three models (tasks 6.3–6.5), so its controls
    /// snap to the models' active or staged values.
    fn rerender_theme(&self) {
        if let Some(page) = self.theme.borrow().as_ref() {
            page.rerender();
        }
    }
}

/// The shared, persistent handles the window's chrome and page (re)population operate
/// on (task 5.4).
///
/// The window's structure is built once (see the module docs); this bundle is what the
/// chrome button closures and the startup/refresh flows capture so they can repopulate
/// the stack and re-derive the chrome without rebuilding the shell. It reads as the
/// window, the shared store and detection state, the per-category [`PageModels`] and
/// [`MountedPages`], and the chrome. It is [`Clone`] because every field is a refcounted
/// handle (GTK objects, `Rc`s, the `Rc<dyn Fn>` chrome refresher, and the refcounted
/// [`Toast`]), so a clone shares the same underlying state.
#[derive(Clone)]
struct Shell {
    /// The top-level window, used as the transient parent for modal warnings.
    window: ApplicationWindow,
    /// The single staging store every page and the chrome share (task 5.3).
    store: Rc<RefCell<SettingsStore>>,
    /// The most recent detection result, replaced on a manual refresh (R4.3).
    capabilities: Rc<RefCell<Capabilities>>,
    /// Whether a load pass ([`spawn_load`] — the startup load or a manual refresh) is
    /// currently running on the worker (task 9.3).
    ///
    /// Consulted by [`Shell::update_chrome`], which disables chrome while it is set:
    /// **Refresh**, so at most one pass is ever in flight — a refresh clicked during
    /// the initial startup load could otherwise land first and be clobbered when
    /// [`Shell::apply_startup_load`] assigns the models wholesale and re-baselines
    /// the store from the (by then stale) startup snapshot; and **Apply/Reset**, so a
    /// commit cannot land between the worker snapshotting the files and the merge of
    /// its result — [`Shell::apply_refresh_load`] would otherwise adopt clean,
    /// pre-Apply models and visibly revert the UI. Set by whichever code spawns a
    /// load, cleared when its result is handled.
    load_in_flight: Rc<Cell<bool>>,
    /// The bespoke per-category models (staging sources and write helpers).
    models: PageModels,
    /// The pages currently mounted in `stack`, with their controllers and dirty markers.
    pages: MountedPages,
    /// The persistent content stack whose category pages are (re)populated in place.
    stack: Stack,
    /// Re-derives all chrome from the store and the staging models (Apply/Reset/Refresh
    /// enablement + per-page markers). Shared as the pages' `on_changed` and called after
    /// every staged-state change.
    update_chrome: Rc<dyn Fn()>,
    /// The non-fatal reload-failure toast (R5.5).
    toast: Toast,
}

/// Builds the top-level [`ApplicationWindow`], returning it immediately with a loading
/// placeholder while detection and config parsing run on a worker thread (task
/// 5.1/5.3/5.4, architecture §8).
///
/// This is what task 1.3's bootstrap presents. The whole shell — titlebar, chrome, and
/// an empty stack showing a loading placeholder — is assembled here, before the window
/// is presented (so the titlebar is set on an unrealized window). The shared store
/// starts empty and the capabilities start [absent](Capabilities::absent);
/// [`Shell::start_load`] kicks off the worker, and [`Shell::apply_startup_load`]
/// populates the store and stack on the main thread when it completes. Returning before
/// that finishes is the whole point — the window maps at once and fills in when ready,
/// which keeps cold start inside the R8.1 budget.
pub(crate) fn build(app: &Application) -> ApplicationWindow {
    let window = ApplicationWindow::builder()
        .application(app)
        .title(WINDOW_TITLE)
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .build();

    // The single store every page and the chrome share (see the module docs). Empty
    // until the worker delivers the parsed config files; capabilities start absent so
    // no category shows during the (momentary) loading state.
    let store = Rc::new(RefCell::new(SettingsStore::new()));
    let capabilities = Rc::new(RefCell::new(Capabilities::absent()));
    // The bespoke models and the mounted-page handles all start empty: the worker fills
    // the models and `populate` mounts the pages (see the two structs' docs).
    let models = PageModels::default();
    let pages = MountedPages::default();

    // The persistent content stack + sidebar. Pages are added to this same stack on
    // every populate; the stack itself is never rebuilt.
    let stack = Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    let sidebar = StackSidebar::new();
    sidebar.set_stack(&stack);

    // Chrome widgets. Built once and wired once — the pages and stack they act on are
    // persistent, so their handlers never need re-wiring across a repopulate.
    let apply_button = Button::with_label("Apply");
    // A built-in theme style class (not custom CSS, R2.1) that renders Apply as the
    // primary/affirmative action.
    apply_button.add_css_class("suggested-action");
    let reset_button = Button::with_label("Reset");
    let refresh_button = Button::with_label("Refresh");
    let toast = Toast::new();

    // Whether a worker load pass is in flight (task 9.3; see the Shell field docs).
    // Starts false; `start_load` sets it (and re-derives the chrome) before spawning.
    let load_in_flight = Rc::new(Cell::new(false));

    // Re-derives all chrome from the store: Apply/Reset/Refresh enablement and each
    // page's dirty marker (R5.1). Captures the persistent stack + marked, so it is
    // built once and reused across repopulates.
    let update_chrome: Rc<dyn Fn()> = {
        let store = store.clone();
        let models = models.clone();
        let apply_button = apply_button.clone();
        let reset_button = reset_button.clone();
        let refresh_button = refresh_button.clone();
        let load_in_flight = load_in_flight.clone();
        let stack = stack.clone();
        let marked = pages.marked.clone();
        Rc::new(move || {
            let store = store.borrow();
            // The Display and Theme pages are additional dirty sources (tasks 6.1/6.3–6.5):
            // none is a store value (monitors are dynamic; a palette switch runs
            // `generate-colors`; the GTK/icon/cursor themes and the wallpaper/lock paths
            // are bespoke models), so each reports its own dirty state, OR-ed into the
            // shared Apply/Reset enablement.
            let display_dirty = models.display_dirty();
            let theme_dirty = models.theme_dirty();
            let enabled = chrome::actions_enabled(&store) || display_dirty || theme_dirty;
            // While a worker load pass runs, all three chrome actions disable (task
            // 9.3): Refresh so only one pass is ever in flight, and Apply/Reset so a
            // commit cannot race the worker's file snapshot (see the `load_in_flight`
            // field docs). The dirty markers below still update — staged edits remain
            // visible, they just cannot be acted on until the pass lands.
            let in_flight = load_in_flight.get();
            apply_button.set_sensitive(enabled && !in_flight);
            reset_button.set_sensitive(enabled && !in_flight);
            refresh_button.set_sensitive(!in_flight);
            for (category, child) in marked.borrow().iter() {
                // Display reads its own model; Theme ORs its three bespoke models with any
                // store-backed Theme setting, so the marker stays correct should one be
                // added. Every other page reads the store's per-category rollup.
                let dirty = match *category {
                    Category::Display => display_dirty,
                    Category::Theme => theme_dirty || store.is_category_dirty(Category::Theme),
                    other => store.is_category_dirty(other),
                };
                stack.page(child).set_needs_attention(dirty);
            }
        })
    };

    // Show the loading placeholder immediately (architecture §8): a non-sidebar stack
    // page the worker's result replaces via `populate`.
    let loading = build_loading_page();
    stack.add_named(&loading, Some(LOADING_PAGE_NAME));
    stack.set_visible_child_name(LOADING_PAGE_NAME);

    // Assemble the content, floated under the toast so it can appear over the pages.
    let content = GtkBox::new(Orientation::Horizontal, 0);
    content.append(&sidebar);
    content.append(&stack);
    let overlay = Overlay::new();
    overlay.set_child(Some(&content));
    overlay.add_overlay(toast.revealer());

    // The application ("hamburger") menu holding the About entry (task 10.2, R2.5). Its
    // item fires the `app.about` action that `super::app` registers on the application.
    let menu_button = MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&about::menu_model(&AboutInfo::from_cargo()))
        .tooltip_text("Main menu")
        .build();

    // `pack_end` fills from the right edge inward, so the first end-packed widget sits
    // outermost: the menu button goes at the far right, as in GNOME applications, with
    // Apply beside it and Reset to Apply's left.
    let header = HeaderBar::new();
    header.pack_start(&refresh_button);
    header.pack_end(&menu_button);
    header.pack_end(&apply_button);
    header.pack_end(&reset_button);

    // Install the shell once, before the window is presented — the titlebar can only be
    // set on an unrealized window (see the module docs).
    window.set_titlebar(Some(&header));
    window.set_child(Some(&overlay));

    let shell = Shell {
        window: window.clone(),
        store,
        capabilities,
        load_in_flight,
        models,
        pages,
        stack,
        update_chrome,
        toast,
    };

    shell.wire_apply(&apply_button);
    shell.wire_reset(&reset_button);
    shell.wire_refresh(&refresh_button);
    shell.wire_page_entries();

    // Initial (disabled) chrome state, then kick off the worker — `start_load` marks
    // the pass in flight and re-derives the chrome again, so Refresh is already
    // disabled by the time the window is presented (task 9.3).
    (shell.update_chrome)();
    shell.start_load();

    window
}

/// Connects a page-entry hook that runs `probe` on the page currently held in `page`
/// whenever `category`'s stack child becomes visible — but from the GTK main loop's
/// **idle queue**, not from the notification itself (task 9.4).
///
/// This is the one mechanism behind every runtime page's entry probe; which pages use it,
/// and what each probe reads, is listed in [`Shell::wire_page_entries`].
///
/// # Why the idle hop
///
/// The runtime-backed pages (Sound, Notifications, Network) read their live state by
/// spawning a short-lived command, so the probe must never sit on the populate path: a
/// wedged PipeWire, session bus or NetworkManager holds one for the full 5 s command
/// timeout ([`CommandRunner`](crate::system::command::CommandRunner)), which would delay
/// every later category's appearance against the R8.1 cold-start budget. Deferring the *first*
/// read out of the page's `build` is not enough on its own, because
/// [`Stack::add_titled`] makes the first page added the
/// visible one and emits `notify::visible-child-name` **synchronously**: when the page
/// belongs to the first visible category (e.g. Sound on a host without `hyprctl`, where
/// the sidebar starts at Sound), a probe run straight from the handler would execute
/// inside [`Shell::populate`] after all. Handing it to the idle queue guarantees
/// `populate` returns — and the page's placeholder paints — before anything is spawned.
/// It also means the widget tree is never mutated from inside `add_titled`.
///
/// # What the callback re-checks
///
/// Everything, because the world can change between the notification and the idle run: a
/// repopulate may have replaced or dropped the page (the callback reads whatever handle
/// `page` holds *then*), and the user may have moved to another category, in which case
/// there is nothing to probe. Both handles are held weakly, so a pending callback cannot
/// keep the window alive after it closes. While one callback is outstanding no second is
/// queued — flipping to the page and back before the idle runs must not spawn two probes.
///
/// The notification handler itself is connected once, to the stack the window keeps for
/// its whole lifetime, so it survives every repopulate without handlers accumulating.
fn wire_page_entry<P: 'static>(
    stack: &Stack,
    page: &Rc<RefCell<Option<P>>>,
    category: SidebarCategory,
    probe: fn(&P),
) {
    let stack_name = category.stack_name();
    // Set while an idle callback for this page is queued, so repeated entries coalesce.
    let queued = Rc::new(Cell::new(false));
    let page = Rc::downgrade(page);

    stack.connect_visible_child_name_notify(move |stack| {
        if stack.visible_child_name().as_deref() != Some(stack_name) {
            return;
        }
        if queued.replace(true) {
            return;
        }

        let stack = stack.downgrade();
        let page = page.clone();
        let queued = queued.clone();
        glib::idle_add_local_once(move || {
            queued.set(false);
            let Some(stack) = stack.upgrade() else {
                return;
            };
            // The user may have left the page again while this callback waited; a probe
            // for a page nobody is looking at is exactly what the deferral avoids.
            if stack.visible_child_name().as_deref() != Some(stack_name) {
                return;
            }
            let Some(page) = page.upgrade() else {
                return;
            };
            if let Some(page) = page.borrow().as_ref() {
                probe(page);
            }
        });
    });
}

impl Shell {
    /// Runs detection + config parsing on a worker thread and applies the result on the
    /// main thread when it completes (task 5.4, architecture §8; R4.3, R8.1).
    ///
    /// The blocking work goes through [`spawn_load`] so it runs concurrently with the
    /// main thread returning to the GTK loop, which then populates the store and builds
    /// the pages — GTK is only ever touched on the main thread. While the pass runs the
    /// `load_in_flight` guard keeps Refresh disabled, so a user-initiated refresh can
    /// never interleave with the startup pass (task 9.3; the guard clears — and the
    /// chrome re-arms via `populate`'s chrome refresh — when the result is handled).
    /// If the worker finishes without delivering a result (e.g. the window closed
    /// first), the stack is still populated (empty, from the absent capabilities) so
    /// the user has a working Refresh (R4.3).
    fn start_load(&self) {
        // Mark the pass in flight and re-derive the chrome so Refresh/Apply/Reset
        // disable before the window is even presented (see the field docs).
        self.load_in_flight.set(true);
        (self.update_chrome)();

        let shell = self.clone();
        spawn_load(move |load| {
            shell.load_in_flight.set(false);
            match load {
                Some(load) => shell.apply_startup_load(load),
                None => {
                    tracing::warn!(
                        "startup worker finished without delivering a result; showing an empty \
                         window (R4.3)"
                    );
                    // Still populate (empty, from the absent capabilities) so the user
                    // has a working Refresh to retry.
                    shell.populate();
                }
            }
        });
    }

    /// Applies a completed [`StartupLoad`] on the main thread: populate the store from
    /// the parsed config files, adopt the detected capabilities, and build the pages
    /// (task 5.4).
    ///
    /// This is the "populate on completion" step. Each parsed file is fed to
    /// [`SettingsStore::load_file`], which records its originals and the freshness
    /// baseline (from the exact bytes read) and keeps a reader for the conflict-reload
    /// path (R5.6). Then [`Shell::populate`] builds the stack pages for the detected
    /// capabilities, so the categories/rows the machine supports appear and the rest are
    /// cleanly hidden (R4.2). A one-line summary is logged at `info` (architecture §8,
    /// R7.3) — visible-category counts only, never file contents; per-file parse issues
    /// were already logged by the loader.
    fn apply_startup_load(&self, load: StartupLoad) {
        let StartupLoad {
            capabilities,
            files,
            display,
            input,
            notifications,
            power,
            palette,
            themes,
            wallpaper,
        } = load;
        let file_count = files.len();

        // Populate the store with the real originals + freshness baselines (R5.1/R5.6).
        {
            let mut store = self.store.borrow_mut();
            for LoadedFile {
                path,
                initial,
                reader,
            } in files
            {
                store.load_file(path, initial, reader);
            }
        }
        *self.capabilities.borrow_mut() = capabilities;
        // Adopt the worker-built models (see `PageModels`) before `populate` builds their
        // pages.
        let models = &self.models;
        *models.display.borrow_mut() = display;
        *models.input.borrow_mut() = input;
        *models.notifd_config.borrow_mut() = notifications;
        *models.power.borrow_mut() = power;
        *models.palette.borrow_mut() = palette;
        *models.themes.borrow_mut() = themes;
        *models.wallpaper.borrow_mut() = wallpaper;

        self.populate();

        // R7.3 / architecture §8: a one-line startup summary — which categories are
        // visible and how many backing files loaded — counts only, never file contents.
        let caps = self.capabilities.borrow();
        let visible: Vec<&str> = visible_categories(&caps)
            .iter()
            .map(|category| category.title())
            .collect();
        tracing::info!(
            files_loaded = file_count,
            categories = visible.len(),
            visible = ?visible,
            "startup load complete; store populated and pages built (task 5.4, architecture §8)"
        );
    }

    /// (Re)builds the stack's category pages from the current capabilities (task
    /// 5.1/5.4; R4.2).
    ///
    /// Called on startup-load completion and again on every manual detection refresh
    /// (R4.3): the persistent stack has all its pages removed (the loading placeholder
    /// and any pages from a previous populate, since categories/rows may appear or
    /// disappear) and rebuilt for the current capabilities. The shared store persists,
    /// so staged edits are preserved; the previous page controllers are dropped (their
    /// widgets removed with the old pages) and fresh ones retained.
    fn populate(&self) {
        let caps = self.capabilities.borrow().clone();

        // Drop any retained Display/Sound/Notifications/Theme/Network pages from a
        // previous populate; their `populate_*` helpers re-set them when the categories
        // are (re)built below.
        //
        // This happens *before* the stack is torn down on purpose: removing the visible
        // page makes GTK pick another child as the visible one, which fires the
        // page-entry hooks ([`Self::wire_page_entries`]). With the handles already
        // cleared those hooks find no page to refresh, so tearing the stack down can
        // never run a `pw-dump`/`nmcli`/`busctl` probe for a page that is on its way out
        // (task 9.4).
        self.pages.release_bespoke();

        // Remove every current stack page (the loading placeholder, and any pages from a
        // previous populate).
        while let Some(child) = self.stack.first_child() {
            self.stack.remove(&child);
        }
        self.pages.marked.borrow_mut().clear();

        // Build one page per visible category, in sidebar order (R4.2). The match is
        // exhaustive on purpose: a new category does not compile until it is given a
        // builder here. Each builder's docs say why its category is built that way.
        let mut controllers = Vec::new();
        for category in visible_categories(&caps) {
            match category {
                // Bespoke, store-free staging pages, rendered from their own models.
                SidebarCategory::Display => self.populate_display(category),
                SidebarCategory::Theme => self.populate_theme(category),
                // Runtime-only or read-only pages, probed from their page-entry hooks.
                SidebarCategory::Sound => self.populate_sound(category),
                SidebarCategory::Network => self.populate_network(category),
                // Framework rows that need model data `plan_category` cannot supply;
                // Notifications also wraps them in its runtime-only notifd controls.
                SidebarCategory::Input => self.populate_input(category, &mut controllers),
                SidebarCategory::Notifications => {
                    self.populate_notifications(category, &mut controllers);
                }
                // Pure framework pages: their rows come straight from `plan_category`,
                // annotated with the notes the startup load found in hypridle.conf (an
                // ambiguous `general { }` block, task 9.31).
                SidebarCategory::PowerAndIdle => {
                    let notes = self
                        .models
                        .power
                        .borrow()
                        .as_ref()
                        .map(|power| power.setting_notes().to_vec())
                        .unwrap_or_default();
                    self.populate_framework(category, &caps, &notes, &mut controllers);
                }
            }
        }

        // Refresh the chrome for the new page set, and retain the fresh controllers so
        // the window can send them `Rerender` (dropping any from a previous populate).
        (self.update_chrome)();
        *self.pages.controllers.borrow_mut() = controllers;
    }

    /// Adds `child` to the stack as `category`'s sidebar page, under its stack name and
    /// title.
    fn add_category_page(&self, category: SidebarCategory, child: &impl IsA<Widget>) {
        self.stack
            .add_titled(child, Some(category.stack_name()), category.title());
    }

    /// Builds a category page purely from the declarative row framework (task 5.2).
    ///
    /// [`page::plan_category`] decides between a framework page (its controller retained
    /// in `controllers`, its dirty marker registered), the task-5.1 placeholder for a
    /// category with no row descriptors, and dropping a category whose rows are all gated
    /// out (R4.2; `plan_category` logs that case). `notes` are the configuration notes the
    /// startup load produced for the category (task 9.31); the plan places each below
    /// the first visible row it concerns.
    fn populate_framework(
        &self,
        category: SidebarCategory,
        caps: &Capabilities,
        notes: &[SettingNote],
        controllers: &mut Vec<Controller<SettingsPage>>,
    ) {
        // The plan reads the store too: some rows exist only for certain loaded
        // configurations (the lock grace period, task 11.13). The borrow ends with the
        // `plan_category` call, before `build_page` takes its own handle.
        let plan = page::plan_category(category, caps, &self.store.borrow(), notes);
        match plan {
            PagePlan::Framework(rows) => {
                let controller =
                    page::build_page(self.store.clone(), rows, self.update_chrome.clone());
                let root = controller.widget().clone();
                self.add_category_page(category, &root);
                if let Some(model_category) = chrome::marker_category(category) {
                    self.pages.mark(model_category, root.upcast());
                }
                controllers.push(controller);
            }
            PagePlan::NoSpec => {
                self.add_category_page(category, &build_placeholder_page(category));
            }
            // Every row gated out: drop the category (R4.2); `plan_category` logged it.
            PagePlan::Emptied => {}
        }
    }

    /// Builds the Display category's page (task 6.1).
    ///
    /// Display is bespoke rather than a framework page because its per-monitor controls
    /// are known only at runtime and its laptop-display toggle is runtime-only (R5.2).
    /// When a live [`DisplayModel`] is available, mounts the bespoke
    /// [`display`] page (rendering from the shared model and reporting staged edits
    /// through `update_chrome`) and registers its dirty marker under
    /// [`Category::Display`]. When there is no live monitor data — `hyprctl` is present
    /// (so the category is visible, task 5.1) but the compositor is not reloadable —
    /// it falls back to the task-5.1 placeholder so the page degrades cleanly (R4.2).
    fn populate_display(&self, category: SidebarCategory) {
        if self.models.display.borrow().is_some() {
            let page = display::build(self.models.display.clone(), self.update_chrome.clone());
            self.add_category_page(category, page.root());
            // The Display page's marker tracks its own model's dirty state, not the
            // store's (it holds no store-backed settings).
            self.pages
                .mark(Category::Display, page.root().clone().upcast());
            *self.pages.display.borrow_mut() = Some(page);
        } else {
            self.add_category_page(category, &build_placeholder_page(category));
        }
    }

    /// Builds the Input category's page (tasks 6.6, 11.4).
    ///
    /// The Input settings are ordinary store-backed [`SettingId`](crate::core::model::SettingId)s,
    /// so this uses the declarative row framework like the generic path — the only reason
    /// it is special-cased is that its keyboard-layout add-control is populated from the
    /// runtime XKB candidates the [`InputModel`] holds ([`input::page_rows`]), which the
    /// pure `page::plan_category` cannot supply. The rows are gated per-row (R4.2) and, if
    /// any survive, a [`SettingsPage`] is launched and its controller retained in
    /// `controllers` (so a Reset/commit re-renders it via [`PageMsg::Rerender`], and its
    /// dirty marker tracks the store's Input rollup like any framework page). Without an
    /// [`InputModel`] there are no rows and the category is dropped (R4.2): the startup
    /// load builds none when `input.lua` is missing or holds lines the editor does not
    /// understand, and has already logged why.
    fn populate_input(
        &self,
        category: SidebarCategory,
        controllers: &mut Vec<Controller<SettingsPage>>,
    ) {
        let caps = self.capabilities.borrow();
        let visible = input::page_rows(self.models.input.borrow().as_ref(), &caps);
        if visible.is_empty() {
            tracing::info!(
                category = category.title(),
                "the Input page has no rows (no usable input.lua, or every row gated out); \
                 hiding the whole category (R4.2)"
            );
            return;
        }

        let controller = page::build_page(self.store.clone(), visible, self.update_chrome.clone());
        let root = controller.widget().clone();
        self.add_category_page(category, &root);
        // The Input marker tracks the store's per-page dirty rollup, like any framework
        // page (its settings are all store-backed).
        self.pages.mark(Category::Input, root.upcast());
        controllers.push(controller);
    }

    /// Builds the Notifications category's page (tasks 11.7, 11.15).
    ///
    /// Notifications is bespoke because its runtime controls drive notifd state directly
    /// (R5.2). Mounts the bespoke [`notifications`] page. When the startup load built the notifd
    /// settings helper, the staged rows ([`notifications::page_rows`]) are launched as a
    /// framework [`SettingsPage`] — its controller kept in `controllers` so a Reset or
    /// commit re-renders it — and placed inside the page, and the page's dirty marker
    /// follows [`Category::Notifications`]; the runtime controls never feed the
    /// Apply/Reset chrome (R5.2). Without the helper the runtime controls stand alone
    /// (the startup load logged why). The category is only reached when detection found
    /// notifd running or installed (task 5.1's gate). Building runs no `busctl`: the
    /// first read is the page-entry hook's ([`Self::wire_page_entries`]), and
    /// — as on the Sound and Network pages — the handle is stored *before* the page joins
    /// the stack, because adding the first page fires the entry hooks synchronously and
    /// every hook is written to find the page it is about to refresh.
    fn populate_notifications(
        &self,
        category: SidebarCategory,
        controllers: &mut Vec<Controller<SettingsPage>>,
    ) {
        let rows = notifications::page_rows(self.models.notifd_config.borrow().as_ref());
        let staged = (!rows.is_empty())
            .then(|| page::build_page(self.store.clone(), rows, self.update_chrome.clone()));
        let page = notifications::build(
            staged
                .as_ref()
                .map(|controller| controller.widget().upcast_ref::<Widget>()),
        );
        let root = page.root().clone();
        *self.pages.notifications.borrow_mut() = Some(page);
        self.add_category_page(category, &root);
        if let Some(controller) = staged {
            if let Some(model_category) = chrome::marker_category(category) {
                self.pages.mark(model_category, root.upcast());
            }
            controllers.push(controller);
        }
    }

    /// Builds the Sound category's page (task 6.2).
    ///
    /// Sound is bespoke because every control is runtime-only (R5.2), so nothing is staged
    /// in the store. Mounts the bespoke [`sound`] page, which loads the live PipeWire
    /// devices and renders the runtime-only output/input controls. No dirty marker is registered: the
    /// page stages nothing and never feeds the Apply/Reset chrome (R5.2). The category is
    /// only reached here when the Sound gate found `wpctl` present (task 5.1), so the
    /// device load and controls have their client.
    ///
    /// Building runs no `pw-dump`/`wpctl`: the first load is deferred to the page's
    /// first entry (the [`Self::wire_page_entries`] hook, which runs it from the idle
    /// queue), so a wedged PipeWire can never stall populate — and with it every
    /// category's appearance — against the R8.1 startup budget (task 9.4). As on the
    /// Network and Notifications pages the handle is stored *before* the page is added to
    /// the stack: adding the first page makes it the visible child, which fires the entry
    /// hooks synchronously, and every hook is written to find the page it is about to
    /// refresh.
    fn populate_sound(&self, category: SidebarCategory) {
        let page = sound::build();
        let root = page.root().clone();
        *self.pages.sound.borrow_mut() = Some(page);
        self.add_category_page(category, &root);
    }

    /// Builds the Network category's page (task 6.9).
    ///
    /// Network is bespoke because it is read-only, runtime-backed status (R3.1) with no
    /// store-backed settings. Mounts the bespoke [`network`] page, which shows the active NetworkManager
    /// connections and offers the "Open Network Settings" launcher button. Which
    /// tool that button spawns is decided here from the current capabilities
    /// (nm-connection-editor preferred, else kitty+nmtui, else no button — R4.2). No
    /// dirty marker is registered: the page is read-only and runtime-backed, so it
    /// never feeds the Apply/Reset chrome (R3.1). The category is only reached when
    /// `nmcli` is present (task 5.1).
    ///
    /// Building runs no `nmcli`: the first status read is deferred to the page's first
    /// entry (the [`Self::wire_page_entries`] hook, which runs it from the idle
    /// queue), so a wedged NetworkManager can never stall populate — and with it every
    /// category's appearance — against the R8.1 startup budget. The handle is stored
    /// *before* the page is added to the stack: adding the first page makes it the visible
    /// child, which fires the entry hooks synchronously, and every hook is written to find
    /// the page it is about to refresh.
    fn populate_network(&self, category: SidebarCategory) {
        let launcher = crate::core::network::launcher(&self.capabilities.borrow());
        let page = network::build(launcher);
        let root = page.root().clone();
        *self.pages.network.borrow_mut() = Some(page);
        self.add_category_page(category, &root);
    }

    /// Builds the Theme category's page (tasks 6.3/6.4).
    ///
    /// Theme is bespoke because none of its values is a store setting: the palette switch
    /// runs `generate-colors`, and the theme and wallpaper changes are multi-file writes
    /// owned by their models. The page hosts three independent sections, each present when its model was built
    /// by the worker: the palette-scheme section when the dotfiles palette source was
    /// detected (R3.2/R8.5), the GTK/icon/cursor appearance section when `gsettings`
    /// is present (R3.3/R3.4/R4.2), and the wallpaper section when hyprpaper is present
    /// (task 6.5, R4.2). The bespoke [`theme`] page is mounted whenever *any* model
    /// exists (rendering from the shared models and reporting staged changes through
    /// `update_chrome`), and its dirty marker is registered under [`Category::Theme`].
    /// When no model exists the page degrades to the task-5.1 placeholder (R4.2/R4.4);
    /// the reason is logged at `info`, matching the hidden-item convention
    /// (detection/startup also log *why* each model is absent).
    fn populate_theme(&self, category: SidebarCategory) {
        let models = &self.models;
        let has_palette = models.palette.borrow().is_some();
        let has_themes = models.themes.borrow().is_some();
        let has_wallpaper = models.wallpaper.borrow().is_some();
        if has_palette || has_themes || has_wallpaper {
            let page = theme::build(
                models.palette.clone(),
                models.themes.clone(),
                models.wallpaper.clone(),
                self.update_chrome.clone(),
            );
            self.add_category_page(category, page.root());
            // The Theme marker tracks the three Theme models' dirty state (and any
            // store-backed Theme setting, should one be added).
            self.pages
                .mark(Category::Theme, page.root().clone().upcast());
            *self.pages.theme.borrow_mut() = Some(page);
        } else {
            tracing::info!(
                "Theme sections hidden: no dotfiles palette source, no gsettings, and no \
                 hyprpaper (R3.2/R3.3/R4.2/R8.5)"
            );
            self.add_category_page(category, &build_placeholder_page(category));
        }
    }

    /// Connects the page-entry hook of every runtime-backed page, each through the one
    /// deferred mechanism [`wire_page_entry`] (tasks 6.2, 6.9, 9.4, 11.7).
    ///
    /// Each hook re-reads its page's live state whenever the page becomes the visible
    /// stack child, so the page reflects changes made elsewhere while the app showed
    /// another page — and the same hook performs the page's deferred *first* read, since
    /// none of these pages' `build` runs a command: the page shows its placeholder until
    /// the hook fires on first entry. A repopulate therefore never re-probes a page
    /// nobody is viewing: [`Self::populate`] rebuilds it in its placeholder state and the
    /// hook reads when the user next opens it. Per page:
    ///
    /// - **Sound** loads the PipeWire devices, volumes and mute state (R3.1).
    /// - **Notifications** reads the notifd daemon's state — do-not-disturb, count,
    ///   inhibited — including a daemon that started since the page was last shown (R5.2,
    ///   the rule task 9.26 asked for).
    /// - **Network** re-reads the NetworkManager connection status (R3.1).
    ///
    /// A new runtime page adds one line here. Each hook is connected once, to the
    /// persistent stack, and reads whichever page its handle holds when it fires.
    fn wire_page_entries(&self) {
        let pages = &self.pages;
        wire_page_entry(
            &self.stack,
            &pages.sound,
            SidebarCategory::Sound,
            SoundPage::refresh,
        );
        wire_page_entry(
            &self.stack,
            &pages.notifications,
            SidebarCategory::Notifications,
            NotificationsPage::refresh,
        );
        wire_page_entry(
            &self.stack,
            &pages.network,
            SidebarCategory::Network,
            NetworkPage::refresh,
        );
    }

    /// Wires the Apply button: assemble the plan, run the pipeline, dispatch the outcome
    /// (R5.3–R5.6).
    ///
    /// The handler is deliberately thin, because everything it *decides* is decided in
    /// GTK-free, headlessly tested code (R6.2):
    ///
    /// - [`assemble_apply_plan`] folds every staging source into one plan — the store's
    ///   dirty settings as validations (R8.3) plus each page's rendered file writes — or
    ///   refuses to plan the Apply at all, in which case nothing is written, every staged
    ///   edit is kept, and the reason is surfaced through [`chrome::assembly_warning`];
    ///   for a conflict (R5.6) that happens after [`Self::refresh_conflicted_source`] has
    ///   adopted what is now on disk.
    /// - [`apply::run`] is given the **store's** freshness tracker, so its own step-2
    ///   conflict check measures the store's files against the baselines the startup load
    ///   recorded (R5.6) rather than an empty tracker.
    /// - [`chrome::respond_to_apply`] maps the outcome to commit-or-warn; a commit is
    ///   reconciled by [`Self::commit_applied`] and may raise a non-fatal toast (R5.5),
    ///   which also names any staged value a file refused, while an Apply whose every
    ///   staged value was refused is a warning rather than a silent commit (task 9.28).
    fn wire_apply(&self, apply_button: &Button) {
        let shell = self.clone();
        apply_button.connect_clicked(move |_| {
            let runner = SystemCommandRunner::new();

            // Every staging source, borrowed together and read-only: assembly never
            // mutates one, so an abort below cannot have disturbed a staged edit. The
            // borrows end with this block, before a commit or reload borrows mutably.
            let assembled = {
                let models = &shell.models;
                let store = shell.store.borrow();
                let display = models.display.borrow();
                let input = models.input.borrow();
                let notifd_config = models.notifd_config.borrow();
                let power = models.power.borrow();
                let palette = models.palette.borrow();
                let themes = models.themes.borrow();
                let wallpaper = models.wallpaper.borrow();
                assemble_apply_plan(ApplySources {
                    store: &store,
                    display: display.as_ref(),
                    input: input.as_ref(),
                    notifications: notifd_config.as_ref(),
                    power: power.as_ref(),
                    palette: palette.as_ref(),
                    themes: themes.as_ref(),
                    wallpaper: wallpaper.as_ref(),
                })
            };
            let assembled = match assembled {
                Ok(assembled) => assembled,
                Err(failure) => {
                    // Nothing was written either way. A conflicted model additionally has
                    // to be reloaded from disk before the dialog tells the user to
                    // re-apply against the new contents; a write that could not be
                    // prepared changed nothing at all, so there is nothing to reload.
                    if let PrepareFailure::Conflict(source) = failure {
                        shell.refresh_conflicted_source(source, &runner);
                    }
                    let warning = chrome::assembly_warning(&failure);
                    chrome::show_warning(&shell.window, &warning.heading, &warning.detail);
                    return;
                }
            };

            // The side-effect seams: the real system runner (created above) plus the
            // signaller for the reloads that signal a process rather than run a command.
            let signaller = SystemProcessSignaller::new();
            let outcome = {
                let store = shell.store.borrow();
                let caps = shell.capabilities.borrow();
                apply::run(
                    &assembled.plan,
                    store.freshness(),
                    &caps,
                    &runner,
                    &signaller,
                )
            };

            match chrome::respond_to_apply(&outcome, &assembled) {
                ApplyResponse::Commit { toast: message } => {
                    shell.commit_applied(&assembled.store_writes, assembled.commits);
                    if let Some(message) = message {
                        shell.toast.show(&message);
                    }
                    tracing::info!(
                        "apply committed; store, Display, palette, themes, and wallpaper \
                         reconciled (task 5.3/6.1/6.3/6.4/6.5)"
                    );
                }
                ApplyResponse::Warn { heading, detail } => {
                    chrome::show_warning(&shell.window, &heading, &detail);
                }
            }
        });
    }

    /// Reconciles every staging source with what an
    /// [`ApplyOutcome::Applied`](crate::core::apply::ApplyOutcome::Applied) apply put on
    /// disk, then re-renders the pages and re-derives the chrome (task 5.3/6.1/6.3–6.5).
    ///
    /// The store and the bespoke models commit separately because they track freshness
    /// separately: `store_writes` are the files the store loaded, so it promotes their
    /// staged values and re-baselines their freshness from the bytes just written (task
    /// 4.5) — without which the app's own write looks like an external edit on the next
    /// Apply — while each model re-baselines the files it owns inside its own `commit`.
    /// `commits` says which models actually contributed to the plan; committing one that
    /// wrote nothing would promote staged edits against contents never written.
    fn commit_applied(&self, store_writes: &[(PathBuf, Vec<u8>)], commits: ModelCommits) {
        self.store.borrow_mut().commit_apply(store_writes);
        // The Display model promotes its staged monitor edits and updates its in-memory
        // records (task 6.1).
        if commits.display {
            if let Some(display) = self.models.display.borrow_mut().as_mut() {
                display.commit();
            }
        }
        // The palette model promotes the staged scheme to active, so the palette control
        // is clean again (task 6.3). There is no on-disk baseline to re-record: the app
        // does not write the generated `colors.conf`, `generate-colors` does.
        if commits.palette {
            if let Some(palette) = self.models.palette.borrow_mut().as_mut() {
                palette.commit();
            }
        }
        // The GTK/icon/cursor model promotes its staged selections and re-baselines its
        // backing files' freshness from the just-written bytes (task 6.4).
        if commits.themes {
            if let Some(themes) = self.models.themes.borrow_mut().as_mut() {
                themes.commit();
            }
        }
        // The wallpaper / lock-background model does the same for `hyprpaper.conf` and
        // `hyprlock.conf` (task 6.5).
        if commits.wallpaper {
            if let Some(wallpaper) = self.models.wallpaper.borrow_mut().as_mut() {
                wallpaper.commit();
            }
        }
        self.pages.rerender_all();
        (self.update_chrome)();
    }

    /// Refreshes the staging model whose backing files changed on disk — replacing it with
    /// the model's own `reload` of the same sources — re-renders its page, and re-derives
    /// the chrome (R5.6).
    ///
    /// Called when [`assemble_apply_plan`] refuses to plan an Apply because a file a
    /// model owns was changed by another program since the model read it: rather than
    /// write over that edit, the app adopts what is now on disk and asks the user to
    /// re-apply, which discards the pending edits to that model (they were made against
    /// contents that no longer exist).
    ///
    /// The three arms differ in more than which model they touch, which is why this is a
    /// `match` rather than one generic helper: the Display model is rebuilt by probing the
    /// compositor, so its reload needs a [`CommandRunner`] and may yield nothing at all
    /// (the compositor went away — the same state as a cold start without one), while the
    /// two Theme models only re-read files and always produce a model. The Display model
    /// also drives its own page, whereas both Theme models share the Theme page.
    fn refresh_conflicted_source(&self, source: ConflictedSource, runner: &dyn CommandRunner) {
        match source {
            ConflictedSource::Display => {
                let reloaded = self
                    .models
                    .display
                    .borrow()
                    .as_ref()
                    .and_then(|model| model.reload(runner));
                *self.models.display.borrow_mut() = reloaded;
                self.pages.rerender_display();
            }
            ConflictedSource::Themes => {
                let reloaded = self
                    .models
                    .themes
                    .borrow()
                    .as_ref()
                    .map(ThemesModel::reload);
                *self.models.themes.borrow_mut() = reloaded;
                self.pages.rerender_theme();
            }
            ConflictedSource::Wallpaper => {
                let reloaded = self
                    .models
                    .wallpaper
                    .borrow()
                    .as_ref()
                    .map(WallpaperModel::reload);
                *self.models.wallpaper.borrow_mut() = reloaded;
                self.pages.rerender_theme();
            }
        }
        (self.update_chrome)();
    }

    /// Wires the Reset button to discard staged edits and revert the controls (R5.1).
    fn wire_reset(&self, reset_button: &Button) {
        let shell = self.clone();
        reset_button.connect_clicked(move |_| {
            shell.store.borrow_mut().reset();
            // Reset the bespoke staging sources too (tasks 6.1/6.3–6.5) — they are
            // additional dirty sources, so Reset must clear them alongside the store.
            shell.models.reset_staged();
            shell.pages.rerender_all();
            (shell.update_chrome)();
            tracing::debug!("discarded staged edits from the Reset button (R5.1)");
        });
    }

    /// Wires the Refresh button to re-run the full startup load — detection, the
    /// backing files, and the bespoke page models — and repopulate from the result
    /// (task 9.3; R4.3/R5.6).
    ///
    /// Refresh must rebuild the same state startup built, not just re-run detection:
    /// [`Shell::populate`] renders the Display/Theme sections and the Input candidates
    /// from the worker-built models, so refreshed capabilities without refreshed models
    /// would leave e.g. a newly installed hyprpaper's wallpaper section hidden until an
    /// app restart. The pass runs on the worker exactly like cold start ([`spawn_load`])
    /// — it probes `hyprctl` and scans the theme roots, too slow for the main thread —
    /// under the shared `load_in_flight` guard: while it runs, Refresh disables (loads
    /// can never overlap, including with the startup pass) and Apply/Reset disable (a
    /// commit cannot land between the worker's file snapshot and the merge — the merge
    /// would otherwise adopt clean pre-Apply models and visibly revert the UI). On
    /// completion [`Shell::apply_refresh_load`] merges the result and repopulates.
    fn wire_refresh(&self, refresh_button: &Button) {
        let shell = self.clone();
        refresh_button.connect_clicked(move |_| {
            // Mark the pass in flight and re-derive the chrome so Refresh/Apply/Reset
            // disable until the result is handled (see the `load_in_flight` docs).
            shell.load_in_flight.set(true);
            (shell.update_chrome)();

            let shell = shell.clone();
            spawn_load(move |load| {
                shell.load_in_flight.set(false);
                match load {
                    Some(load) => shell.apply_refresh_load(load),
                    // No result only happens while the window is closing; re-derive
                    // the chrome anyway so the buttons re-arm regardless.
                    None => (shell.update_chrome)(),
                }
            });
        });
    }

    /// Applies a completed manual-refresh pass on the main thread: merge the re-read
    /// files into the store, adopt the new capabilities and page models, repopulate the
    /// stack, and warn about external edits (task 9.3; R4.3/R5.6).
    ///
    /// The refresh counterpart of [`Shell::apply_startup_load`], differing in what it
    /// must preserve. The store keeps its pending staged edits:
    /// [`merge_refreshed_files`] reloads externally-changed tracked files
    /// through [`SettingsStore::refresh`] (reporting them for the warning below) and
    /// only *newly appeared* files are loaded fresh. The staging models (Display,
    /// palette, GTK/icon/cursor themes, wallpaper) go through
    /// [`adopt_refreshed_model`], which keeps a model holding staged edits
    /// rather than clobbering in-progress work. The Input/Notifications/Power helpers
    /// hold no staged state (their settings stage in the shared store), so they are
    /// simply replaced.
    fn apply_refresh_load(&self, load: StartupLoad) {
        let StartupLoad {
            capabilities,
            files,
            display,
            input,
            notifications,
            power,
            palette,
            themes,
            wallpaper,
        } = load;

        // R5.6: reload externally-changed tracked files (keeping pending edits) and
        // start tracking files that appeared since the last load.
        let report = merge_refreshed_files(&mut self.store.borrow_mut(), files);

        // R4.3: adopt the refreshed capabilities and models before `populate` rebuilds
        // the pages from them.
        *self.capabilities.borrow_mut() = capabilities;
        let models = &self.models;
        adopt_refreshed_model(
            &mut models.display.borrow_mut(),
            display,
            DisplayModel::is_dirty,
        );
        *models.input.borrow_mut() = input;
        *models.notifd_config.borrow_mut() = notifications;
        *models.power.borrow_mut() = power;
        adopt_refreshed_model(
            &mut models.palette.borrow_mut(),
            palette,
            PaletteModel::is_dirty,
        );
        adopt_refreshed_model(
            &mut models.themes.borrow_mut(),
            themes,
            ThemesModel::is_dirty,
        );
        adopt_refreshed_model(
            &mut models.wallpaper.borrow_mut(),
            wallpaper,
            WallpaperModel::is_dirty,
        );

        // Repopulate the stack for the new capabilities and models (categories,
        // sections, and rows may appear or disappear).
        self.populate();

        if let Some(warning) = chrome::refresh_conflict_warning(&report) {
            chrome::show_warning(&self.window, &warning.heading, &warning.detail);
        }
        tracing::info!(
            "manual refresh complete: detection, backing files, and page models re-read \
             (R4.3/R5.6)"
        );
    }
}

/// Builds the loading placeholder shown while the startup worker runs (task 5.4).
///
/// A centred spinner and label so the window reads as "working" rather than empty
/// during the brief window before detection + parsing complete. It uses only plain
/// GTK4 widgets and inherits the system theme (R2.1); [`Shell::populate`] removes it
/// from the stack once the real pages arrive.
fn build_loading_page() -> GtkBox {
    let content = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    content.set_halign(Align::Center);
    content.set_valign(Align::Center);
    content.set_hexpand(true);
    content.set_vexpand(true);

    let spinner = Spinner::new();
    spinner.set_halign(Align::Center);
    spinner.start();
    content.append(&spinner);

    let label = Label::new(Some("Detecting installed applications…"));
    label.set_halign(Align::Center);
    content.append(&label);

    content
}

/// Builds the placeholder content for one category page (task 5.1).
///
/// Shown for a category that is visible but has nothing to render: the Display page
/// without live monitor data, the Theme page without any of its models, or a framework
/// category with no row descriptors ([`PagePlan::NoSpec`]). The page is a
/// [`ScrolledWindow`] wrapping a vertical box that holds a single label with the
/// category title, so the page still renders as distinct, non-empty content and
/// scrolls like every real page at the small logical sizes R2.4 targets.
fn build_placeholder_page(category: SidebarCategory) -> ScrolledWindow {
    let content = GtkBox::new(Orientation::Vertical, SECTION_SPACING);
    content.set_margin_top(PAGE_MARGIN);
    content.set_margin_bottom(PAGE_MARGIN);
    content.set_margin_start(PAGE_MARGIN);
    content.set_margin_end(PAGE_MARGIN);

    // Left-aligned so the heading starts at the page's leading edge, like the section
    // headings of the real pages, rather than centring.
    let heading = Label::new(Some(category.title()));
    heading.set_halign(Align::Start);
    content.append(&heading);

    let scroller = ScrolledWindow::new();
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&content));
    scroller
}
