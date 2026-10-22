//! The application menu's **About** entry and the About window it opens (task 10.2,
//! R2.5).
//!
//! # What this module is
//!
//! R2.5 asks for an app menu in the header bar whose About entry shows the
//! application name and icon, the version, the author, and a clickable link to the
//! project's GitHub page — with every displayed value derived from the crate's package
//! metadata, so the About window, the CLI `--version` and the startup log can never
//! disagree (R8.6). The module splits that the same way the rest of `ui/` does:
//!
//! - **The content** — [`AboutInfo`], a pure, GTK-free value built by
//!   [`AboutInfo::from_cargo`] from Cargo's compile-time `CARGO_PKG_*` variables plus
//!   the two identifiers the rest of the UI already owns (the window title and the
//!   GApplication ID). Every string the window shows comes from it, so the content is
//!   unit-tested headlessly (R6.2) and no literal can drift from `Cargo.toml`.
//! - **The widgets** — [`menu_model`] (the one-item menu the header bar's hamburger
//!   button carries) and [`show_about`] (the window), which only render an
//!   [`AboutInfo`].
//!
//! # Why a plain window and not `GtkAboutDialog`
//!
//! `gtk::AboutDialog` is deprecated from GTK 4.10, and the crate enables gtk4's
//! `v4_10` feature, so using it would raise a deprecation warning that fails the
//! `cargo clippy -- -D warnings` gate; silencing that with `allow(deprecated)` would
//! build on an API slated for removal. libadwaita's `AdwAboutDialog` is out too: the
//! app does not use libadwaita at all, because it hard-codes the Adwaita stylesheet
//! and ignores the user's GTK theme (R2.1). So, like the warning dialog in
//! [`super::chrome`], the About window is a plain modal [`Window`] built from stock
//! widgets. Its only styling is the built-in `dim-label` theme class and Pango markup
//! for the heading — neither is custom CSS, so `tests/no_custom_css.rs` stays green.

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, IconTheme, Image, Label, LinkButton, NamedAction, Orientation, Shortcut,
    ShortcutController, ShortcutTrigger, Window, gio, glib,
};

use super::app::APP_ID;
use super::window::WINDOW_TITLE;

/// The name of the application-scoped action that opens the About window.
///
/// Registered on the `Application` (see `super::app`), so the menu item refers to it
/// with the `app.` prefix ([`ABOUT_DETAILED_ACTION`]).
pub(super) const ABOUT_ACTION: &str = "about";

/// [`ABOUT_ACTION`] as the menu item addresses it: GIO resolves an `app.`-prefixed
/// name against the application's own action group.
const ABOUT_DETAILED_ACTION: &str = "app.about";

/// Pixel size of the application icon in the About window.
///
/// The icon is a scalable SVG, so any size renders crisply; 128 px matches what
/// desktop About windows conventionally show and stays modest beside the text.
const ICON_PIXEL_SIZE: i32 = 128;

/// Everything the About window displays, assembled from compile-time metadata (R2.5).
///
/// Kept free of GTK types so the content is testable without a display. The fields
/// are `&'static str` / owned strings derived only from `env!` values and constants
/// the rest of the UI already uses — never from literals of their own — so bumping
/// the version or changing the repository URL in `Cargo.toml` updates the window with
/// no other edit (R8.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AboutInfo {
    /// The human-facing application name, shared with the main window's title so the
    /// two cannot spell it differently. (The crate name, `settings4000`, is the
    /// lower-case binary name and not what users see.)
    pub(crate) name: &'static str,
    /// The semantic version from `Cargo.toml` (`CARGO_PKG_VERSION`), the same string
    /// `settings4000 --version` prints.
    pub(crate) version: &'static str,
    /// The authors from `Cargo.toml`, joined for display (see [`format_authors`]).
    pub(crate) authors: String,
    /// The project's repository URL from `Cargo.toml` (`CARGO_PKG_REPOSITORY`); the
    /// window's link opens it.
    pub(crate) repository: &'static str,
    /// The icon name to look up in the icon theme. It is the GApplication ID because
    /// that is the name the installer gives the icon file
    /// (`hicolor/scalable/apps/<app-id>.svg`) and the name the desktop entry's `Icon=`
    /// key carries (task 8.2), so the About window shows the same icon as the
    /// launcher.
    pub(crate) icon_name: &'static str,
}

impl AboutInfo {
    /// Builds the About content from the crate's compile-time package metadata.
    ///
    /// The `env!` reads happen when the crate is compiled, so the values are fixed
    /// into the binary and cannot fail at runtime. Task 10.1's guard test in
    /// `super::app` already fails the build's tests if `authors` or `repository` is
    /// missing from `Cargo.toml`, which is what keeps these fields from rendering
    /// blank.
    pub(crate) fn from_cargo() -> Self {
        Self {
            name: WINDOW_TITLE,
            version: env!("CARGO_PKG_VERSION"),
            authors: format_authors(env!("CARGO_PKG_AUTHORS")),
            repository: env!("CARGO_PKG_REPOSITORY"),
            icon_name: APP_ID,
        }
    }

    /// The label of the menu item that opens the window, e.g. "About Settings4000".
    pub(crate) fn menu_label(&self) -> String {
        format!("About {}", self.name)
    }

    /// The About window's title, which its title bar shows.
    pub(crate) fn window_title(&self) -> String {
        self.menu_label()
    }

    /// The version line shown under the name, e.g. "Version 1.0.0".
    pub(crate) fn version_line(&self) -> String {
        format!("Version {}", self.version)
    }

    /// The author line, e.g. "Created by FiBe4000".
    pub(crate) fn authors_line(&self) -> String {
        format!("Created by {}", self.authors)
    }
}

/// Turns Cargo's `CARGO_PKG_AUTHORS` value into a display string.
///
/// Cargo joins the `authors` array of `Cargo.toml` with `:` when it sets the
/// variable, so a multi-author crate would otherwise show as `Alice:Bob`. Empty
/// entries are dropped and the rest joined with `", "`.
fn format_authors(raw: &str) -> String {
    raw.split(':')
        .map(str::trim)
        .filter(|author| !author.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the header bar's application menu: a single "About Settings4000" item bound
/// to the `app.about` action (R2.5).
///
/// The item is enabled only while that action is registered on the application, which
/// happens when the main window is built (`super::app`).
pub(crate) fn menu_model(info: &AboutInfo) -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some(&info.menu_label()), Some(ABOUT_DETAILED_ACTION));
    menu
}

/// Shows the About window, modal over and transient for `parent` (R2.5).
///
/// Layout, top to bottom, all centred: the application icon, the name as a bold
/// heading, the version, the author line, and a [`LinkButton`] to the repository. The
/// window is closed from its own title bar's close button or with Escape; it is
/// destroyed with its parent, so it can never outlive the main window.
///
/// # The icon
///
/// The icon is looked up by name ([`AboutInfo::icon_name`], the GApplication ID) in
/// the current icon theme, which finds the copy `install.sh` places in the user's
/// `hicolor` tree — the same file the launcher shows. When the app runs uninstalled
/// (for example from `cargo run`) no icon by that name exists, and GTK would draw its
/// broken-image placeholder instead; the icon is then simply left out rather than
/// showing a placeholder.
///
/// # The link
///
/// [`LinkButton`] opens its URI with the desktop's default handler (through the
/// OpenURI portal when one is available) and shows the URI as its tooltip. Its label
/// is the URL itself, so the reader sees exactly where the click leads.
pub(crate) fn show_about(parent: &impl IsA<Window>, info: &AboutInfo) {
    let window = Window::builder()
        .title(info.window_title())
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .resizable(false)
        .build();

    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(36);
    content.set_margin_end(36);
    content.set_halign(Align::Center);

    // `GtkWindow` is both a widget and a root, and both traits offer `display()`;
    // they return the same display, so take the widget one.
    let display = WidgetExt::display(parent.as_ref());
    if IconTheme::for_display(&display).has_icon(info.icon_name) {
        let icon = Image::from_icon_name(info.icon_name);
        icon.set_pixel_size(ICON_PIXEL_SIZE);
        content.append(&icon);
    } else {
        tracing::debug!(
            icon_name = info.icon_name,
            "the application icon is not installed in the icon theme; the About window omits it"
        );
    }

    // Pango markup, not CSS, for the heading — it changes only the font of this label
    // and leaves colours to the theme (R2.1). The name is escaped so a future name
    // containing `&` or `<` cannot break the markup.
    let name = Label::new(None);
    name.set_markup(&format!(
        "<span size=\"x-large\" weight=\"bold\">{}</span>",
        glib::markup_escape_text(info.name)
    ));

    let version = Label::new(Some(&info.version_line()));
    // A built-in theme style class (not custom CSS) that renders secondary text.
    version.add_css_class("dim-label");

    let authors = Label::new(Some(&info.authors_line()));

    let link = LinkButton::with_label(info.repository, info.repository);

    content.append(&name);
    content.append(&version);
    content.append(&authors);
    content.append(&link);
    window.set_child(Some(&content));

    // A plain `GtkWindow` does not close on Escape (only the deprecated `GtkDialog`
    // did), so bind it to the window's built-in `window.close` action to give the
    // About window the keyboard dismissal users expect from a dialog.
    let shortcuts = ShortcutController::new();
    shortcuts.add_shortcut(Shortcut::new(
        ShortcutTrigger::parse_string("Escape"),
        Some(NamedAction::new("window.close")),
    ));
    window.add_controller(shortcuts);

    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_info_is_built_from_the_cargo_metadata() {
        // R2.5/R8.6: every displayed value comes from Cargo's package metadata or a
        // constant the rest of the UI already uses, never a literal of its own.
        let info = AboutInfo::from_cargo();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.repository, env!("CARGO_PKG_REPOSITORY"));
        assert!(
            info.repository
                .starts_with("https://github.com/FiBe4000/Settings4000"),
            "the About link must point at the GitHub project, got `{}`",
            info.repository
        );
        assert!(
            !info.authors.is_empty(),
            "the author line must not be blank"
        );
        assert!(info.authors.contains("FiBe4000"));
        assert_eq!(info.name, WINDOW_TITLE);
    }

    #[test]
    fn the_icon_name_is_the_one_the_desktop_entry_installs() {
        // The installer names the icon file after the GApplication ID and the
        // desktop entry's `Icon=` key carries the same value (task 8.2, pinned by
        // the guard in `super::app`); reading the desktop entry here as well proves
        // the About window looks up exactly the launcher's icon.
        let info = AboutInfo::from_cargo();
        assert_eq!(info.icon_name, APP_ID);

        let desktop_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join(format!("{APP_ID}.desktop"));
        let desktop = std::fs::read_to_string(&desktop_path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", desktop_path.display()));
        let expected = format!("Icon={}", info.icon_name);
        assert!(
            desktop.lines().any(|line| line == expected),
            "{} must contain `{expected}`",
            desktop_path.display()
        );
    }

    #[test]
    fn displayed_lines_embed_the_metadata() {
        let info = AboutInfo::from_cargo();
        assert_eq!(info.menu_label(), format!("About {WINDOW_TITLE}"));
        assert_eq!(info.window_title(), info.menu_label());
        assert_eq!(
            info.version_line(),
            format!("Version {}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(info.authors_line(), format!("Created by {}", info.authors));
    }

    #[test]
    fn multiple_authors_are_joined_for_display() {
        // Cargo joins the `authors` array with `:`; the window must not show that
        // separator.
        assert_eq!(format_authors("FiBe4000"), "FiBe4000");
        assert_eq!(
            format_authors("Alice <a@example.org>:Bob"),
            "Alice <a@example.org>, Bob"
        );
        assert_eq!(format_authors(":Alice::"), "Alice");
        assert_eq!(format_authors(""), "");
    }

    #[test]
    fn the_menu_item_targets_the_application_action() {
        // The menu addresses the action through the `app.` prefix; if the registered
        // name and the menu's target ever diverged, the item would silently grey out.
        assert_eq!(ABOUT_DETAILED_ACTION, format!("app.{ABOUT_ACTION}"));
    }
}
