//! The main window: the category sidebar and content stack (task 5.1), the
//! Apply/Reset chrome, dirty markers, toast, and detection refresh (task 5.3), and
//! the worker-thread startup sequencing that populates them (task 5.4; architecture
//! §7, §8; R2.1, R2.4, R4.2, R4.3, R5.1, R5.5, R5.6, R8.1).
//!
//! # What this builds
//!
//! The window has a [`HeaderBar`] titlebar carrying the chrome — a **Refresh** button,
//! a **Reset** button, and a suggested-action **Apply** button — over a content area
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
//! — to a worker thread via [`super::startup::load`]. The parsed
//! [`StartupLoad`](super::startup::StartupLoad) is delivered back to the main thread
//! over a channel and applied by [`Shell::apply_startup_load`]: it populates the shared
//! store from the real config files (establishing the true `original` values and
//! freshness baselines) and rebuilds the stack pages from the detected
//! [`Capabilities`]. A missing binary/daemon, an unreadable/unparseable config, or an
//! absent repo never blocks or crashes startup — detection degrades to absent and the
//! loader skips the file (R4.3/R4.4) — so the window always comes up with whatever is
//! available.
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
//! The Display page (task 6.1) contributes the first real file write: [`wire_apply`]
//! folds its `monitors.conf` [`FileWrite`](crate::core::apply::FileWrite) and value
//! validations into the same plan, so an Apply rewrites the target `monitor=` record
//! and reloads via `hyprctl reload`. The store's remaining §6 pages will fill in their
//! own writes the same way; until then their staged edits still validate and commit as
//! before.
//!
//! # No libadwaita, no custom CSS (R2.1/R2.2)
//!
//! Only plain GTK4 widgets are used — never libadwaita, and never a `CssProvider` or
//! any custom-CSS mechanism. The Apply button's `suggested-action` and the toast's
//! `osd` are *built-in* theme style classes applied with `add_css_class`, which selects
//! styling the active theme already provides rather than shipping our own; the rule is
//! enforced by `tests/no_custom_css.rs`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, HeaderBar, Label, Orientation,
    Overlay, ScrolledWindow, Spinner, Stack, StackSidebar, Widget,
};
use relm4::{ComponentController, Controller};

use crate::core::apply::{self, ApplyPlan};
use crate::core::detect::{Capabilities, DetectionInputs};
use crate::core::display::DisplayModel;
use crate::core::model::Category;
use crate::core::reload::ReloadParams;
use crate::core::store::SettingsStore;
use crate::core::theme::PaletteModel;
use crate::system::command::SystemCommandRunner;
use crate::system::signal::SystemProcessSignaller;
use crate::ui::category::{SidebarCategory, visible_categories};
use crate::ui::chrome::{self, ApplyResponse, Toast};
use crate::ui::display::{self, DisplayPage};
use crate::ui::page::{self, PageMsg, PagePlan, SettingsPage};
use crate::ui::sound::{self, SoundPage};
use crate::ui::startup::{self, LoadedFile, StartupLoad};
use crate::ui::theme::{self, ThemePage};

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

/// The `GtkStack` name of the loading placeholder page shown while the worker runs.
///
/// It is added with a name but no title, so [`StackSidebar`] does not show a row for
/// it — the sidebar is empty during loading — and [`Shell::populate`] removes it once
/// the real pages arrive.
const LOADING_PAGE_NAME: &str = "__loading";

/// The shared, persistent handles the window's chrome and page (re)population operate
/// on (task 5.4).
///
/// The window's structure is built once (see the module docs); this bundle is what the
/// chrome button closures and the startup/refresh flows capture so they can repopulate
/// the stack and re-derive the chrome without rebuilding the shell. It is [`Clone`]
/// because every field is a refcounted handle (GTK objects, `Rc`s, the `Rc<dyn Fn>`
/// chrome refresher, and the refcounted [`Toast`]), so a clone shares the same
/// underlying state.
#[derive(Clone)]
struct Shell {
    /// The top-level window, used as the transient parent for modal warnings.
    window: ApplicationWindow,
    /// The single staging store every page and the chrome share (task 5.3).
    store: Rc<RefCell<SettingsStore>>,
    /// The Display page's runtime-discovered model (task 6.1), shared with the Display
    /// glue. `None` until the startup worker builds it, or when there is no live
    /// compositor. It is a second staging source alongside the store: its dirty state
    /// feeds the same Apply/Reset chrome, and its `monitors.conf` write is folded into
    /// the same Apply pipeline (see [`Shell::wire_apply`]).
    display: Rc<RefCell<Option<DisplayModel>>>,
    /// The mounted Display page, retained so the window can re-render it after a Reset
    /// or a committed Apply. Rebuilt by [`Shell::populate`]; `None` while the Display
    /// category shows a placeholder (no live monitor data).
    display_page: Rc<RefCell<Option<DisplayPage>>>,
    /// The mounted Sound page (task 6.2), retained so the window can re-enumerate it when
    /// the page is re-shown. Rebuilt by [`Shell::populate`]; `None` when the Sound
    /// category is not visible. It is runtime-only, so — unlike the store and the Display
    /// model — it feeds neither the Apply/Reset chrome nor a dirty marker (R5.2).
    sound_page: Rc<RefCell<Option<SoundPage>>>,
    /// The Theme page's palette-scheme model (task 6.3), shared with the Theme glue.
    /// `None` until the startup worker builds it, or when there is no dotfiles palette
    /// source. Like the Display model it is a staging source alongside the store: its
    /// dirty state feeds the same Apply/Reset chrome, and its scheme switch is folded
    /// into the same Apply pipeline as a `PaletteSwitch` (see [`Shell::wire_apply`]).
    palette: Rc<RefCell<Option<PaletteModel>>>,
    /// The mounted Theme page, retained so the window can re-render it after a Reset or a
    /// committed Apply. Rebuilt by [`Shell::populate`]; `None` while the Theme category
    /// shows a placeholder (no palette source, and no other Theme section built yet).
    theme_page: Rc<RefCell<Option<ThemePage>>>,
    /// The most recent detection result, replaced on a manual refresh (R4.3).
    capabilities: Rc<RefCell<Capabilities>>,
    /// The retained page controllers, so the window can send them [`PageMsg::Rerender`]
    /// and so their widgets stay alive while mounted in the stack. Replaced by
    /// [`Shell::populate`].
    controllers: Rc<RefCell<Vec<Controller<SettingsPage>>>>,
    /// The persistent content stack whose category pages are (re)populated in place.
    stack: Stack,
    /// The `(Category, stack page)` pairs whose `needs-attention` marker tracks per-page
    /// dirty state; refilled by [`Shell::populate`] and read by [`Shell::update_chrome`].
    marked: Rc<RefCell<Vec<(Category, Widget)>>>,
    /// Re-derives all chrome from the store (Apply/Reset enablement + per-page markers).
    /// Shared as the pages' `on_changed` and called after every store change.
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
    let controllers: Rc<RefCell<Vec<Controller<SettingsPage>>>> = Rc::new(RefCell::new(Vec::new()));
    // The Display page's model and mounted page — a second staging source folded into
    // the chrome and the Apply pipeline (task 6.1). Both start empty; the worker fills
    // the model and `populate` builds the page.
    let display: Rc<RefCell<Option<DisplayModel>>> = Rc::new(RefCell::new(None));
    let display_page: Rc<RefCell<Option<DisplayPage>>> = Rc::new(RefCell::new(None));
    // The Sound page (task 6.2) — a runtime-only bespoke page rebuilt by `populate`.
    let sound_page: Rc<RefCell<Option<SoundPage>>> = Rc::new(RefCell::new(None));
    // The Theme page's palette model and mounted page (task 6.3) — a staging source
    // folded into the chrome and the Apply pipeline, like the Display model. Both start
    // empty; the worker fills the model and `populate` builds the page.
    let palette: Rc<RefCell<Option<PaletteModel>>> = Rc::new(RefCell::new(None));
    let theme_page: Rc<RefCell<Option<ThemePage>>> = Rc::new(RefCell::new(None));

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

    let marked: Rc<RefCell<Vec<(Category, Widget)>>> = Rc::new(RefCell::new(Vec::new()));

    // Re-derives all chrome from the store: Apply/Reset enablement and each page's
    // dirty marker (R5.1). Captures the persistent stack + marked, so it is built once
    // and reused across repopulates.
    let update_chrome: Rc<dyn Fn()> = {
        let store = store.clone();
        let display = display.clone();
        let palette = palette.clone();
        let apply_button = apply_button.clone();
        let reset_button = reset_button.clone();
        let stack = stack.clone();
        let marked = marked.clone();
        Rc::new(move || {
            let store = store.borrow();
            // The Display and Theme pages are second/third dirty sources (tasks 6.1/6.3):
            // neither is a store value (monitors are dynamic; a palette switch runs
            // `generate-colors` rather than writing a store-backed setting), so each
            // reports its own dirty state, OR-ed into the shared Apply/Reset enablement.
            let display_dirty = display
                .borrow()
                .as_ref()
                .is_some_and(DisplayModel::is_dirty);
            let palette_dirty = palette
                .borrow()
                .as_ref()
                .is_some_and(PaletteModel::is_dirty);
            let enabled = chrome::actions_enabled(&store) || display_dirty || palette_dirty;
            apply_button.set_sensitive(enabled);
            reset_button.set_sensitive(enabled);
            for (category, child) in marked.borrow().iter() {
                // Display and Theme read their own model's dirty state; Theme also ORs in
                // any store-backed Theme setting so it stays correct once 6.4/6.5 add
                // them. Every other page reads the store's per-category rollup.
                let dirty = match *category {
                    Category::Display => display_dirty,
                    Category::Theme => palette_dirty || store.is_category_dirty(Category::Theme),
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

    let header = HeaderBar::new();
    header.pack_start(&refresh_button);
    header.pack_end(&apply_button);
    header.pack_end(&reset_button);

    // Install the shell once, before the window is presented — the titlebar can only be
    // set on an unrealized window (see the module docs).
    window.set_titlebar(Some(&header));
    window.set_child(Some(&overlay));

    let shell = Shell {
        window: window.clone(),
        store,
        display,
        display_page,
        sound_page,
        palette,
        theme_page,
        capabilities,
        controllers,
        stack,
        marked,
        update_chrome,
        toast,
    };

    shell.wire_apply(&apply_button);
    shell.wire_reset(&reset_button);
    shell.wire_refresh(&refresh_button);
    shell.wire_sound_page_entry();

    // Initial (disabled) chrome state, then kick off the worker.
    (shell.update_chrome)();
    shell.start_load();

    window
}

impl Shell {
    /// Runs detection + config parsing on a worker thread and applies the result on the
    /// main thread when it completes (task 5.4, architecture §8; R4.3, R8.1).
    ///
    /// The blocking work goes to relm4's shared blocking pool ([`relm4::spawn_blocking`])
    /// so it runs concurrently with the main thread returning to the GTK loop; the parsed
    /// [`StartupLoad`] is delivered back over a [`relm4::channel`] and awaited on the main
    /// thread by a local future ([`relm4::spawn_local`]), which then populates the store
    /// and builds the pages — GTK is only ever touched on the main thread. If the worker
    /// finishes without delivering a result (e.g. the window closed first), the stack is
    /// still populated (empty, from the absent capabilities) so the user has a working
    /// Refresh (R4.3).
    fn start_load(&self) {
        let (sender, receiver) = relm4::channel::<StartupLoad>();
        relm4::spawn_blocking(move || {
            let load = startup::load();
            // A send failure only means the receiver was dropped — the window closed
            // before the load finished, which is harmless, so it is not an error.
            if sender.send(load).is_err() {
                tracing::debug!("startup load discarded; window closed before it completed");
            }
        });

        let shell = self.clone();
        relm4::spawn_local(async move {
            match receiver.recv().await {
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
            palette,
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
        // Adopt the worker-built Display and palette models (tasks 6.1/6.3) before
        // `populate` builds their pages from them.
        *self.display.borrow_mut() = display;
        *self.palette.borrow_mut() = palette;

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

        // Remove every current stack page (the loading placeholder, and any pages from a
        // previous populate).
        while let Some(child) = self.stack.first_child() {
            self.stack.remove(&child);
        }
        self.marked.borrow_mut().clear();
        // Drop any retained Display/Sound/Theme pages from a previous populate; their
        // `populate_*` helpers re-set them when the categories are (re)built below.
        *self.display_page.borrow_mut() = None;
        *self.sound_page.borrow_mut() = None;
        *self.theme_page.borrow_mut() = None;

        // Build one page per visible category, in sidebar order (R4.2).
        let mut controllers = Vec::new();
        for category in visible_categories(&caps) {
            // Display is bespoke (task 6.1): its per-monitor controls are dynamic and
            // its laptop toggle is runtime-only, so it does not use the declarative
            // framework. Build it directly from the runtime-discovered model.
            if category == SidebarCategory::Display {
                self.populate_display(category);
                continue;
            }
            // Sound is bespoke too (task 6.2): every control is runtime-only (R5.2), so
            // it does not use the declarative store-backed framework. Build it directly
            // from the runtime-enumerated PipeWire state.
            if category == SidebarCategory::Sound {
                self.populate_sound(category);
                continue;
            }
            // Theme is bespoke as well (task 6.3): its palette switch runs
            // `generate-colors` rather than writing a store-backed setting, so it renders
            // from the [`PaletteModel`] instead of the declarative framework.
            if category == SidebarCategory::Theme {
                self.populate_theme(category);
                continue;
            }
            match page::plan_category(category, &caps) {
                PagePlan::Framework(rows) => {
                    let controller =
                        page::build_page(self.store.clone(), rows, self.update_chrome.clone());
                    let root = controller.widget().clone();
                    self.stack
                        .add_titled(&root, Some(category.stack_name()), category.title());
                    if let Some(model_category) = chrome::marker_category(category) {
                        self.marked
                            .borrow_mut()
                            .push((model_category, root.upcast()));
                    }
                    controllers.push(controller);
                }
                PagePlan::NoSpec => {
                    let placeholder = build_placeholder_page(category);
                    self.stack.add_titled(
                        &placeholder,
                        Some(category.stack_name()),
                        category.title(),
                    );
                }
                // Every row gated out: drop the category (R4.2); `plan_category` logged it.
                PagePlan::Emptied => {}
            }
        }

        // Refresh the chrome for the new page set, and retain the fresh controllers so
        // the window can send them `Rerender` (dropping any from a previous populate).
        (self.update_chrome)();
        *self.controllers.borrow_mut() = controllers;
    }

    /// Builds the Display category's page (task 6.1).
    ///
    /// When a runtime-discovered [`DisplayModel`] is available, mounts the bespoke
    /// [`display`] page (rendering from the shared model and reporting staged edits
    /// through `update_chrome`) and registers its dirty marker under
    /// [`Category::Display`]. When there is no live monitor data — `hyprctl` is present
    /// (so the category is visible, task 5.1) but the compositor is not reloadable —
    /// it falls back to the task-5.1 placeholder so the page degrades cleanly (R4.2).
    fn populate_display(&self, category: SidebarCategory) {
        if self.display.borrow().is_some() {
            let page = display::build(self.display.clone(), self.update_chrome.clone());
            self.stack
                .add_titled(page.root(), Some(category.stack_name()), category.title());
            // The Display page's marker tracks its own model's dirty state, not the
            // store's (it holds no store-backed settings).
            self.marked
                .borrow_mut()
                .push((Category::Display, page.root().clone().upcast()));
            *self.display_page.borrow_mut() = Some(page);
        } else {
            let placeholder = build_placeholder_page(category);
            self.stack
                .add_titled(&placeholder, Some(category.stack_name()), category.title());
        }
    }

    /// Builds the Sound category's page (task 6.2).
    ///
    /// Mounts the bespoke [`sound`] page, which enumerates the live PipeWire devices on
    /// entry and renders the runtime-only output/input controls. No dirty marker is
    /// registered: the page stages nothing and never feeds the Apply/Reset chrome
    /// (R5.2). The category is only reached here when the Sound gate found `wpctl`
    /// present (task 5.1), so the enumeration and controls have their client.
    fn populate_sound(&self, category: SidebarCategory) {
        let page = sound::build();
        self.stack
            .add_titled(page.root(), Some(category.stack_name()), category.title());
        *self.sound_page.borrow_mut() = Some(page);
    }

    /// Builds the Theme category's page (task 6.3).
    ///
    /// Task 6.3 adds only the palette-scheme section, present exactly when detection
    /// discovered the dotfiles palette source (R3.2/R8.5) — the worker builds a
    /// [`PaletteModel`] only then. When it is present the bespoke [`theme`] page is
    /// mounted (rendering from the shared model and reporting staged switches through
    /// `update_chrome`) and its dirty marker is registered under [`Category::Theme`].
    /// When it is absent the palette controls are hidden and the page degrades to the
    /// task-5.1 placeholder (R4.2/R4.4); tasks 6.4/6.5 will build the page for their own
    /// sections too, so this condition grows as they land. The hidden-palette reason is
    /// logged at `info`, matching the hidden-item convention (detection also logs *why*
    /// the source is absent).
    fn populate_theme(&self, category: SidebarCategory) {
        if self.palette.borrow().is_some() {
            let page = theme::build(self.palette.clone(), self.update_chrome.clone());
            self.stack
                .add_titled(page.root(), Some(category.stack_name()), category.title());
            // The Theme marker tracks the palette model's dirty state (and, later, any
            // store-backed Theme setting); it holds no store-backed setting today.
            self.marked
                .borrow_mut()
                .push((Category::Theme, page.root().clone().upcast()));
            *self.theme_page.borrow_mut() = Some(page);
        } else {
            tracing::info!(
                "Theme palette section hidden: no dotfiles palette source (R3.2/R4.2/R8.5)"
            );
            let placeholder = build_placeholder_page(category);
            self.stack
                .add_titled(&placeholder, Some(category.stack_name()), category.title());
        }
    }

    /// Re-enumerates the Sound page whenever it becomes the visible stack child (task
    /// 6.2), so the controls reflect the live audio state on page entry (R3.1) — picking
    /// up volume/device changes made elsewhere while the app was on another page.
    ///
    /// The handler is connected once to the persistent stack (never rebuilt) and reads
    /// the current [`Self::sound_page`] on each change, so it survives every repopulate
    /// without accumulating handlers.
    fn wire_sound_page_entry(&self) {
        let sound_page = self.sound_page.clone();
        self.stack.connect_visible_child_name_notify(move |stack| {
            if stack.visible_child_name().as_deref() == Some(SidebarCategory::Sound.stack_name()) {
                if let Some(page) = sound_page.borrow().as_ref() {
                    page.refresh();
                }
            }
        });
    }

    /// Wires the Apply button to run the pipeline and handle its outcome (R5.3–R5.6).
    ///
    /// See the module docs: the plan is interim (validations only, no writes yet), the
    /// pipeline is given the store's real freshness tracker so its conflict check
    /// measures against the loaded baselines (R5.6), the outcome is dispatched through
    /// [`chrome::respond_to_apply`], and an
    /// [`ApplyOutcome::Applied`](crate::core::apply::ApplyOutcome::Applied) is committed
    /// to the store with the plan's written paths + bytes before the pages are
    /// re-rendered and the chrome refreshed. A non-fatal reload failure raises a toast;
    /// any abort/failure shows a modal warning.
    fn wire_apply(&self, apply_button: &Button) {
        let shell = self.clone();
        apply_button.connect_clicked(move |_| {
            let runner = SystemCommandRunner::new();

            // F2 (R5.6): before writing a pending monitor edit, check whether
            // monitors.conf changed on disk since it was loaded. Only relevant when the
            // Display page is dirty (that is the only case that would write it). On a
            // conflict, warn and re-load the model rather than clobbering the stale
            // parse — the pipeline's own conflict check covers the store's files.
            let display_conflict = {
                let display = shell.display.borrow();
                display
                    .as_ref()
                    .is_some_and(|model| model.is_dirty() && model.check_conflict())
            };
            if display_conflict {
                let reloaded = {
                    let display = shell.display.borrow();
                    display.as_ref().and_then(|model| model.reload(&runner))
                };
                *shell.display.borrow_mut() = reloaded;
                shell.rerender_display();
                (shell.update_chrome)();
                chrome::show_warning(
                    &shell.window,
                    "Files changed on disk",
                    "monitors.conf changed on disk since Settings4000 read it, so nothing was \
                     written. It has been reloaded from disk — re-apply your display changes.",
                );
                return;
            }

            let mut plan = interim_apply_plan(&shell.store.borrow());
            // The store-backed writes to commit to the store after a successful apply.
            // Captured before the Display contribution is folded in, because the Display
            // model commits its own `monitors.conf` write separately (its freshness is
            // owned by the model, not the store).
            let store_writes: Vec<(PathBuf, Vec<u8>)> = plan
                .writes
                .iter()
                .map(|write| (write.path.clone(), write.contents.clone()))
                .collect();

            // Fold in the Display page's `monitors.conf` write + validations (task 6.1)
            // — the first real file write in the app. The immutable borrow ends here,
            // before the commit below borrows the model mutably.
            let has_display_write = {
                let display = shell.display.borrow();
                match display.as_ref().and_then(DisplayModel::apply_contribution) {
                    Some(contribution) => {
                        plan.writes.push(contribution.write);
                        plan.validations.extend(contribution.validations);
                        true
                    }
                    None => false,
                }
            };

            // Fold in the Theme page's palette switch (task 6.3): a staged scheme
            // contributes a `PaletteSwitch`, so the pipeline runs the discovered
            // `generate-colors <scheme>` as its last write step and then the palette
            // reload chain. It writes no file directly — v1 never edits `colors/<scheme>`.
            let has_palette_switch = {
                let palette = shell.palette.borrow();
                match palette.as_ref().and_then(PaletteModel::apply_contribution) {
                    Some(switch) => {
                        plan.palette = Some(switch);
                        true
                    }
                    None => false,
                }
            };

            // Side-effect seams: the real system runner (created above) plus the
            // signaller. The freshness tracker is the store's own (task 5.4), holding
            // the baselines recorded when the startup load read the backing files, so
            // the pipeline's step-2 conflict check measures against them (R5.6).
            let signaller = SystemProcessSignaller::new();
            let outcome = {
                let store = shell.store.borrow();
                let caps = shell.capabilities.borrow();
                apply::run(&plan, store.freshness(), &caps, &runner, &signaller)
            };

            match chrome::respond_to_apply(&outcome) {
                ApplyResponse::Commit { toast: message } => {
                    // The writes stood (R5.5). Commit reconciles each staging source:
                    // the store promotes staged→original and re-baselines its own
                    // written files' freshness (task 4.5); the Display model promotes
                    // its staged monitor edits and updates its in-memory records.
                    shell.store.borrow_mut().commit_apply(&store_writes);
                    if has_display_write {
                        if let Some(display) = shell.display.borrow_mut().as_mut() {
                            display.commit();
                        }
                    }
                    // Promote the staged scheme to active so the palette is clean again
                    // (task 6.3). There is no on-disk baseline to re-record — the app
                    // does not write the generated colors.conf, generate-colors does.
                    if has_palette_switch {
                        if let Some(palette) = shell.palette.borrow_mut().as_mut() {
                            palette.commit();
                        }
                    }
                    shell.rerender_pages();
                    shell.rerender_display();
                    shell.rerender_theme();
                    (shell.update_chrome)();
                    if let Some(message) = message {
                        shell.toast.show(&message);
                    }
                    tracing::info!(
                        "apply committed; store, Display, and palette reconciled (task 5.3/6.1/6.3)"
                    );
                }
                ApplyResponse::Warn { heading, detail } => {
                    chrome::show_warning(&shell.window, &heading, &detail);
                }
            }
        });
    }

    /// Wires the Reset button to discard staged edits and revert the controls (R5.1).
    fn wire_reset(&self, reset_button: &Button) {
        let shell = self.clone();
        reset_button.connect_clicked(move |_| {
            shell.store.borrow_mut().reset();
            // Reset the Display and palette staging too (tasks 6.1/6.3) — they are
            // additional dirty sources, so Reset must clear them alongside the store.
            if let Some(display) = shell.display.borrow_mut().as_mut() {
                display.reset();
            }
            if let Some(palette) = shell.palette.borrow_mut().as_mut() {
                palette.reset();
            }
            shell.rerender_pages();
            shell.rerender_display();
            shell.rerender_theme();
            (shell.update_chrome)();
            tracing::debug!("discarded staged edits from the Reset button (R5.1)");
        });
    }

    /// Wires the Refresh button to re-run detection, repopulate, and warn on conflicts
    /// (R4.3/R5.6).
    ///
    /// It re-detects capabilities, re-reads the tracked files (surfacing external edits
    /// as a conflict warning via [`chrome::refresh_conflict_warning`]), then repopulates
    /// the stack so categories/rows that appeared or disappeared are reflected. Detection
    /// is re-run synchronously here — unlike the cold-start path (task 5.4) it is a
    /// user-initiated action off the startup budget, and re-reading the already-tracked
    /// files is quick.
    fn wire_refresh(&self, refresh_button: &Button) {
        let shell = self.clone();
        refresh_button.connect_clicked(move |_| {
            // R4.3: re-run detection over freshly gathered inputs.
            let inputs = DetectionInputs::from_system(Vec::new());
            *shell.capabilities.borrow_mut() = Capabilities::detect(&inputs);

            // R5.6: re-read the tracked files; an external edit reloads originals
            // (keeping pending edits) and is reported for the warning below.
            let report = shell.store.borrow_mut().refresh();

            // Repopulate the stack for the new capabilities (categories/rows may appear
            // or disappear).
            shell.populate();

            if let Some(warning) = chrome::refresh_conflict_warning(&report) {
                chrome::show_warning(&shell.window, &warning.heading, &warning.detail);
            }
            tracing::info!("manual detection refresh + external-edit check complete (R4.3/R5.6)");
        });
    }

    /// Sends every retained page controller a [`PageMsg::Rerender`] so its controls
    /// re-render from the shared store (task 5.3).
    ///
    /// Used after the window changes the store from outside a page — a Reset or an
    /// applied commit — so a control that no longer matches the store (its original
    /// reverted, or reloaded) snaps back.
    fn rerender_pages(&self) {
        for controller in self.controllers.borrow().iter() {
            // The receiver only goes away if the page's runtime has stopped, which cannot
            // happen while the window holds its controller; ignore the send result rather
            // than unwrap a case that cannot occur here.
            let _ = controller.sender().send(PageMsg::Rerender);
        }
    }

    /// Re-renders the Display page from its model (task 6.1), the bespoke counterpart
    /// of [`Self::rerender_pages`]. Called after a Reset or a committed Apply so the
    /// Display drop-downs snap to the model's values.
    fn rerender_display(&self) {
        if let Some(page) = self.display_page.borrow().as_ref() {
            page.rerender();
        }
    }

    /// Re-renders the Theme page from its palette model (task 6.3), the bespoke
    /// counterpart of [`Self::rerender_pages`]. Called after a Reset or a committed Apply
    /// so the palette drop-down snaps to the model's active/staged scheme.
    fn rerender_theme(&self) {
        if let Some(page) = self.theme_page.borrow().as_ref() {
            page.rerender();
        }
    }
}

/// Builds the base [`ApplyPlan`] from the store's dirty edits (task 5.3).
///
/// It carries the store's dirty settings as `validations` (so [`apply::run`]'s first
/// gate re-checks them, R8.3). It still produces **no** store `writes`: turning a
/// staged [`Value`](crate::core::model::Value) into concrete file bytes goes through
/// the format parsers and is per-page glue, which the remaining §6 pages fill in. The
/// Display page (task 6.1) does not go through here — [`Shell::wire_apply`] folds its
/// `monitors.conf` write and validations into this plan after building it, since the
/// Display staging lives in its own model rather than the store.
fn interim_apply_plan(store: &SettingsStore) -> ApplyPlan {
    let validations = store
        .dirty_ids()
        .into_iter()
        .filter_map(|id| store.value(id).cloned().map(|value| (id, value)))
        .collect();
    ApplyPlan {
        validations,
        writes: Vec::new(),
        palette: None,
        reload_params: ReloadParams::default(),
    }
}

/// Builds the loading placeholder shown while the startup worker runs (task 5.4).
///
/// A centred spinner and label so the window reads as "working" rather than empty
/// during the brief window before detection + parsing complete. It uses only plain
/// GTK4 widgets and inherits the system theme (R2.1); [`Shell::populate`] removes it
/// from the stack once the real pages arrive.
fn build_loading_page() -> GtkBox {
    let content = GtkBox::new(Orientation::Vertical, CONTENT_SPACING);
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
/// The page is a [`ScrolledWindow`] wrapping a vertical box — the shell the real
/// controls plug into in later tasks (§6). The scroller is established now so the
/// content area already scrolls when a page grows taller than the window, keeping it
/// usable at the small logical sizes R2.4 targets. For now the box holds a single label
/// with the category title so each page renders as distinct, non-empty content while
/// the shell is verified.
fn build_placeholder_page(category: SidebarCategory) -> ScrolledWindow {
    let content = GtkBox::new(Orientation::Vertical, CONTENT_SPACING);
    content.set_margin_top(CONTENT_MARGIN);
    content.set_margin_bottom(CONTENT_MARGIN);
    content.set_margin_start(CONTENT_MARGIN);
    content.set_margin_end(CONTENT_MARGIN);

    // Left-aligned so the heading (and the rows §6 adds beneath it) start at the page's
    // leading edge rather than centring.
    let heading = Label::new(Some(category.title()));
    heading.set_halign(Align::Start);
    content.append(&heading);

    let scroller = ScrolledWindow::new();
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&content));
    scroller
}
