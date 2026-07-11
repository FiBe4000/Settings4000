//! The main window: the category sidebar and content stack (task 5.1) plus the
//! Apply/Reset chrome, dirty markers, toast, and detection refresh (task 5.3;
//! architecture §7; R2.1, R2.4, R4.2, R4.3, R5.1, R5.5, R5.6).
//!
//! # What this builds
//!
//! The window has a [`HeaderBar`] titlebar carrying the chrome — a **Refresh** button,
//! a **Reset** button, and a suggested-action **Apply** button — over a content area
//! that is a [`StackSidebar`] beside a [`Stack`] of category pages, all wrapped in a
//! [`Overlay`] so a non-fatal [`Toast`](super::chrome::Toast) can float over it (R5.5).
//! One stack page is added per *visible* category (see [`super::category`]); a
//! [`super::page::PagePlan`] decides whether each is a rendered
//! [`SettingsPage`](super::page::SettingsPage), a task-5.1 placeholder, or dropped
//! entirely when its rows are all gated out (R4.2).
//!
//! # One shared store drives the chrome (task 5.3)
//!
//! Unlike task 5.2 (where each page owned its own store), the window owns a **single**
//! [`SettingsStore`] behind an `Rc<RefCell<…>>` that every page shares. This is what
//! makes the chrome meaningful: an edit on any page stages into the one store, so the
//! Apply/Reset buttons (enabled while [`SettingsStore::is_dirty`]) and the per-page
//! `needs-attention` markers (from [`SettingsStore::is_category_dirty`]) all read the
//! same dirty state. Each page reports an edit through a shared `on_changed` callback
//! so the window re-derives the chrome immediately; the window drives the pages back
//! by sending each retained [`Controller`] a [`PageMsg::Rerender`] after a Reset,
//! commit, or conflict reload.
//!
//! The store is seeded once with the interim demo values ([`page::interim_seed_values`])
//! against a real temporary file ([`SeedSource`]) so [`SettingsStore::refresh`] behaves
//! — task 5.4 replaces this whole seeding with detection + real config parsing on a
//! worker thread.
//!
//! # Apply wires the real pipeline (task 4.5 / 5.3)
//!
//! **Apply** runs [`apply::run`] and dispatches its [`ApplyOutcome`] through the pure
//! [`chrome::respond_to_apply`]: an [`ApplyOutcome::Applied`] is committed to the store
//! (via [`SettingsStore::commit_apply`], which promotes staged→original and re-baselines
//! the written files so the app's own write is not seen as a conflict next time),
//! optionally raising a toast for non-fatal reload failures (R5.5); every other outcome
//! (invalid values, an external conflict, a write failure) is surfaced as a modal
//! warning without committing. The [`ApplyPlan`] is interim — it validates the dirty
//! settings but produces **no** file writes yet, because rendering staged edits through
//! the parsers into file bytes is §6 page glue; the headline here is the chrome, the
//! outcome handling, and the commit wiring.
//!
//! # No libadwaita, no custom CSS (R2.1/R2.2)
//!
//! Only plain GTK4 widgets are used — never libadwaita, and never a `CssProvider` or
//! any custom-CSS mechanism. The Apply button's `suggested-action` and the toast's
//! `osd` are *built-in* theme style classes applied with `add_css_class`, which selects
//! styling the active theme already provides rather than shipping our own; the rule is
//! enforced by `tests/no_custom_css.rs`.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, HeaderBar, Label, Orientation,
    Overlay, ScrolledWindow, Stack, StackSidebar,
};
use relm4::{ComponentController, Controller};
use tempfile::NamedTempFile;

use crate::core::apply::{self, ApplyPlan};
use crate::core::detect::{Capabilities, DetectionInputs};
use crate::core::freshness::FreshnessTracker;
use crate::core::model::Category;
use crate::core::reload::ReloadParams;
use crate::core::store::{FileReader, FileValues, SettingsStore};
use crate::system::command::SystemCommandRunner;
use crate::system::signal::SystemProcessSignaller;
use crate::ui::category::{SidebarCategory, visible_categories};
use crate::ui::chrome::{self, ApplyResponse, Toast};
use crate::ui::page::{self, PageMsg, PagePlan, SettingsPage};

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

/// The marker bytes written into the interim [`SeedSource`] temp file.
///
/// The content is irrelevant — the interim store parses fixed demo values, not this
/// file — but a real, readable, stable file lets [`SettingsStore::refresh`] compare a
/// baseline against unchanged bytes and find no spurious conflict.
const SEED_MARKER: &[u8] = b"settings4000 interim seed\n";

/// The path used for the interim seed when a temp file cannot be created — a synthetic
/// key that is never resolved on disk (the near-impossible fallback, see [`SeedSource`]).
const SEED_FALLBACK_PATH: &str = "<settings4000 interim seed>";

/// Builds the top-level [`ApplicationWindow`] with its chrome and category pages
/// (task 5.1/5.3).
///
/// This is what task 1.3's bootstrap presents. `capabilities` is detected once at
/// startup by the caller (`super::app`); the window keeps its own copy so the manual
/// refresh action (R4.3) can replace it and repopulate. The single shared store is
/// created and seeded here, then [`rebuild`] assembles the shell.
pub(crate) fn build(app: &Application, capabilities: &Capabilities) -> ApplicationWindow {
    let window = ApplicationWindow::builder()
        .application(app)
        .title(WINDOW_TITLE)
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .build();

    // The single store every page and the chrome share (see the module docs). Seeded
    // once here with the interim demo values; task 5.4 replaces this with real config
    // parsing on a worker thread.
    let store = Rc::new(RefCell::new(SettingsStore::new()));
    let seed = Rc::new(SeedSource::new());
    seed_interim_store(&mut store.borrow_mut(), &seed);

    // Held behind `Rc<RefCell<…>>` so the refresh action can replace them and rebuild.
    let capabilities = Rc::new(RefCell::new(capabilities.clone()));
    let controllers: Rc<RefCell<Vec<Controller<SettingsPage>>>> = Rc::new(RefCell::new(Vec::new()));

    rebuild(&window, &store, &capabilities, &controllers, &seed);
    window
}

/// Builds (or rebuilds) the window's chrome + content and installs it on `window`.
///
/// Called once at startup and again on every manual detection refresh (R4.3): a
/// refresh re-detects capabilities into `capabilities`, then calls this to repopulate
/// the sidebar/stack for the new set (categories and rows may appear or disappear). The
/// shared `store` persists across rebuilds, so staged edits are preserved; the previous
/// page controllers are dropped (their widgets removed with the old content) and fresh
/// ones stored in `controllers`.
///
/// The chrome closures capture clones of the shared state, so a fresh Refresh button
/// wired here calls `rebuild` again with the same handles — the recursion happens only
/// on a user click, never at build time.
fn rebuild(
    window: &ApplicationWindow,
    store: &Rc<RefCell<SettingsStore>>,
    capabilities: &Rc<RefCell<Capabilities>>,
    controllers: &Rc<RefCell<Vec<Controller<SettingsPage>>>>,
    seed: &Rc<SeedSource>,
) {
    let caps = capabilities.borrow().clone();

    let stack = Stack::new();
    // The content pane fills the width and height beside the sidebar so pages (and
    // their scrollers) use the whole window (R2.4).
    stack.set_hexpand(true);
    stack.set_vexpand(true);

    let sidebar = StackSidebar::new();
    sidebar.set_stack(&stack);

    // Chrome widgets, created before the pages so `update_chrome` can capture them;
    // their click handlers are wired further down, once the shared state is in hand.
    let apply_button = Button::with_label("Apply");
    // A built-in theme style class (not custom CSS, R2.1) that renders Apply as the
    // primary/affirmative action.
    apply_button.add_css_class("suggested-action");
    let reset_button = Button::with_label("Reset");
    let refresh_button = Button::with_label("Refresh");
    let toast = Toast::new();

    // The (Category, stack child) pairs whose `needs-attention` marker tracks per-page
    // dirty state. Filled during the page loop below and read by `update_chrome` at
    // call time, so it is populated before any edit can fire the callback.
    let marked: Rc<RefCell<Vec<(Category, gtk4::Widget)>>> = Rc::new(RefCell::new(Vec::new()));

    // Re-derives all chrome from the store: Apply/Reset enablement and each page's
    // dirty marker (R5.1). Shared as the pages' `on_changed` and called after every
    // store change the window makes.
    let update_chrome: Rc<dyn Fn()> = {
        let store = store.clone();
        let apply_button = apply_button.clone();
        let reset_button = reset_button.clone();
        let stack = stack.clone();
        let marked = marked.clone();
        Rc::new(move || {
            let store = store.borrow();
            let enabled = chrome::actions_enabled(&store);
            apply_button.set_sensitive(enabled);
            reset_button.set_sensitive(enabled);
            for (category, child) in marked.borrow().iter() {
                stack
                    .page(child)
                    .set_needs_attention(store.is_category_dirty(*category));
            }
        })
    };

    // Build one page per visible category, in sidebar order (R4.2).
    let mut page_controllers = Vec::new();
    for category in visible_categories(&caps) {
        match page::plan_category(category, &caps) {
            PagePlan::Framework(rows) => {
                let controller = page::build_page(store.clone(), rows, update_chrome.clone());
                let root = controller.widget().clone();
                stack.add_titled(&root, Some(category.stack_name()), category.title());
                if let Some(model_category) = chrome::marker_category(category) {
                    marked.borrow_mut().push((model_category, root.upcast()));
                }
                page_controllers.push(controller);
            }
            PagePlan::NoSpec => {
                let placeholder = build_placeholder_page(category);
                stack.add_titled(&placeholder, Some(category.stack_name()), category.title());
            }
            // Every row gated out: drop the category (R4.2); `plan_category` logged it.
            PagePlan::Emptied => {}
        }
    }

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

    wire_apply(
        &apply_button,
        window,
        store,
        capabilities,
        controllers,
        &update_chrome,
        &toast,
    );
    wire_reset(&reset_button, store, controllers, &update_chrome);
    wire_refresh(
        &refresh_button,
        window,
        store,
        capabilities,
        controllers,
        seed,
    );

    window.set_titlebar(Some(&header));
    window.set_child(Some(&overlay));

    // Initial chrome state, and retain the fresh controllers so the window can send
    // them `Rerender` (dropping any from a previous build).
    update_chrome();
    *controllers.borrow_mut() = page_controllers;
}

/// Wires the Apply button to run the pipeline and handle its outcome (R5.3–R5.6).
///
/// See the module docs: the plan is interim (validations only, no writes yet), the
/// outcome is dispatched through [`chrome::respond_to_apply`], and an
/// [`ApplyOutcome::Applied`](crate::core::apply::ApplyOutcome::Applied) is committed to
/// the store with the plan's written paths + bytes before the pages are re-rendered and
/// the chrome refreshed. A non-fatal reload failure raises a toast; any abort/failure
/// shows a modal warning.
fn wire_apply(
    apply_button: &Button,
    window: &ApplicationWindow,
    store: &Rc<RefCell<SettingsStore>>,
    capabilities: &Rc<RefCell<Capabilities>>,
    controllers: &Rc<RefCell<Vec<Controller<SettingsPage>>>>,
    update_chrome: &Rc<dyn Fn()>,
    toast: &Toast,
) {
    let store = store.clone();
    let capabilities = capabilities.clone();
    let controllers = controllers.clone();
    let update_chrome = update_chrome.clone();
    let toast = toast.clone();
    let window = window.clone();
    apply_button.connect_clicked(move |_| {
        let plan = interim_apply_plan(&store.borrow());

        // Side-effect seams: the real system runner/signaller. The freshness tracker is
        // interim-empty because the interim plan writes no tracked file yet (§6 will
        // pass the store's real tracker); with no writes there is nothing to conflict.
        let runner = SystemCommandRunner::new();
        let signaller = SystemProcessSignaller::new();
        let tracker = FreshnessTracker::new();
        let outcome = {
            let caps = capabilities.borrow();
            apply::run(&plan, &tracker, &caps, &runner, &signaller)
        };

        match chrome::respond_to_apply(&outcome) {
            ApplyResponse::Commit { toast: message } => {
                // The writes stood (R5.5). Commit reconciles the store: promote
                // staged→original and re-baseline the written files' freshness from the
                // exact bytes written, so the app's own write is not seen as an external
                // conflict on the next apply (task 4.5). The bytes come from the plan's
                // writes (empty for the interim plan, so this promotes staged→original).
                let committed: Vec<(PathBuf, Vec<u8>)> = plan
                    .writes
                    .iter()
                    .map(|write| (write.path.clone(), write.contents.clone()))
                    .collect();
                store.borrow_mut().commit_apply(&committed);
                rerender_pages(&controllers);
                update_chrome();
                if let Some(message) = message {
                    toast.show(&message);
                }
                tracing::info!("apply committed; store reconciled (task 5.3/4.5)");
            }
            ApplyResponse::Warn { heading, detail } => {
                chrome::show_warning(&window, &heading, &detail);
            }
        }
    });
}

/// Wires the Reset button to discard staged edits and revert the controls (R5.1).
fn wire_reset(
    reset_button: &Button,
    store: &Rc<RefCell<SettingsStore>>,
    controllers: &Rc<RefCell<Vec<Controller<SettingsPage>>>>,
    update_chrome: &Rc<dyn Fn()>,
) {
    let store = store.clone();
    let controllers = controllers.clone();
    let update_chrome = update_chrome.clone();
    reset_button.connect_clicked(move |_| {
        store.borrow_mut().reset();
        rerender_pages(&controllers);
        update_chrome();
        tracing::debug!("discarded staged edits from the Reset button (R5.1)");
    });
}

/// Wires the Refresh button to re-run detection, repopulate, and warn on conflicts
/// (R4.3/R5.6).
///
/// It re-detects capabilities, re-reads the tracked files (surfacing external edits as
/// a conflict warning via [`chrome::refresh_conflict_warning`]), then rebuilds the
/// shell so categories/rows that appeared or disappeared are reflected.
fn wire_refresh(
    refresh_button: &Button,
    window: &ApplicationWindow,
    store: &Rc<RefCell<SettingsStore>>,
    capabilities: &Rc<RefCell<Capabilities>>,
    controllers: &Rc<RefCell<Vec<Controller<SettingsPage>>>>,
    seed: &Rc<SeedSource>,
) {
    let window = window.clone();
    let store = store.clone();
    let capabilities = capabilities.clone();
    let controllers = controllers.clone();
    let seed = seed.clone();
    refresh_button.connect_clicked(move |_| {
        // R4.3: re-run detection over freshly gathered inputs.
        let inputs = DetectionInputs::from_system(Vec::new());
        *capabilities.borrow_mut() = Capabilities::detect(&inputs);

        // R5.6: re-read the tracked files; an external edit reloads originals (keeping
        // pending edits) and is reported for the warning below.
        let report = store.borrow_mut().refresh();

        // Repopulate the sidebar/stack for the new capabilities (categories/rows may
        // appear or disappear). This replaces the content that holds this very button,
        // which GTK tolerates: the emission holds the button until this closure returns.
        rebuild(&window, &store, &capabilities, &controllers, &seed);

        if let Some(warning) = chrome::refresh_conflict_warning(&report) {
            chrome::show_warning(&window, &warning.heading, &warning.detail);
        }
        tracing::info!("manual detection refresh + external-edit check complete (R4.3/R5.6)");
    });
}

/// Sends every retained page controller a [`PageMsg::Rerender`] so its controls
/// re-render from the shared store (task 5.3).
///
/// Used after the window changes the store from outside a page — a Reset, an applied
/// commit, or a conflict reload — so a control that no longer matches the store (its
/// original reverted, or reloaded) snaps back.
fn rerender_pages(controllers: &Rc<RefCell<Vec<Controller<SettingsPage>>>>) {
    for controller in controllers.borrow().iter() {
        // The receiver only goes away if the page's runtime has stopped, which cannot
        // happen while the window holds its controller; ignore the send result rather
        // than unwrap a case that cannot occur here.
        let _ = controller.sender().send(PageMsg::Rerender);
    }
}

/// Builds the interim [`ApplyPlan`] for the current dirty edits (task 5.3).
///
/// It carries the dirty settings as `validations` (so [`apply::run`]'s first gate
/// re-checks them, R8.3) but **no** `writes` and no palette switch: turning a staged
/// [`Value`](crate::core::model::Value) into concrete file bytes goes through the
/// format parsers and is §6 page glue. So a v1 Apply validates and — with nothing to
/// write — completes as
/// [`ApplyOutcome::Applied`](crate::core::apply::ApplyOutcome::Applied), which the
/// caller commits (promoting staged→original, clearing dirty). Task 5.4/§6 fill in the
/// real writes and the store's freshness tracker.
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

/// Seeds the shared store with the interim demo values against the [`SeedSource`] file
/// (task 5.3).
///
/// Registers every framework setting ([`page::interim_seed_values`]) under the seed
/// file's path so the pages have values to render and staging never fails `NotLoaded`.
/// The baseline bytes match the file's on-disk contents so a later
/// [`SettingsStore::refresh`] finds no self-conflict. Interim only — task 5.4 replaces
/// it with real config parsing.
fn seed_interim_store(store: &mut SettingsStore, seed: &SeedSource) {
    let values = page::interim_seed_values();
    let reader_values = values.clone();
    // The reader re-reads the seed file and re-serves the fixed demo values. It is only
    // ever invoked if the seed file's bytes change (they do not), so in practice it is
    // never called; it exists to keep the store parser-agnostic.
    let reader: FileReader = Box::new(move |path: &Path| {
        let bytes = std::fs::read(path)?;
        Ok(FileValues {
            bytes,
            values: reader_values.clone(),
        })
    });
    store.load_file(
        seed.path().to_path_buf(),
        FileValues {
            bytes: seed.baseline_bytes(),
            values,
        },
        reader,
    );
}

/// An interim on-disk backing for the seeded store so [`SettingsStore::refresh`]
/// behaves during task 5.3 (task 5.4 replaces this with the real config files).
///
/// The store's freshness tracker needs a real, readable file to compare against, so
/// the seed values are backed by a [`NamedTempFile`] kept alive here. If the temp file
/// cannot be created (a near-impossible degraded environment), it falls back to a
/// synthetic path: staging still works (the store is loaded), but a manual refresh may
/// then report the seed path as an external change — an acceptable degradation for a
/// case that essentially never occurs.
struct SeedSource {
    /// The path the store tracks the seed under — the temp file, or the fallback.
    path: PathBuf,
    /// The temp file, kept alive so it is not deleted; `None` in the fallback.
    _file: Option<NamedTempFile>,
}

impl SeedSource {
    /// Creates the interim seed, preferring a real temp file (see the type docs).
    fn new() -> Self {
        match create_seed_file() {
            Ok(file) => SeedSource {
                path: file.path().to_path_buf(),
                _file: Some(file),
            },
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not create the interim seed temp file; external-edit refresh may be \
                     inaccurate until real config loading (task 5.4)"
                );
                SeedSource {
                    path: PathBuf::from(SEED_FALLBACK_PATH),
                    _file: None,
                }
            }
        }
    }

    /// The path the seed is tracked under.
    fn path(&self) -> &Path {
        &self.path
    }

    /// The bytes matching the seed file's on-disk contents, for the freshness baseline.
    fn baseline_bytes(&self) -> Vec<u8> {
        if self._file.is_some() {
            SEED_MARKER.to_vec()
        } else {
            Vec::new()
        }
    }
}

/// Creates and populates the interim seed temp file.
fn create_seed_file() -> io::Result<NamedTempFile> {
    use std::io::Write;
    let mut file = NamedTempFile::new()?;
    file.write_all(SEED_MARKER)?;
    file.flush()?;
    Ok(file)
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
