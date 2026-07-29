//! Enforces the hard layering rule from `docs/architecture.md` §2: the
//! `core/` and `parsers/` modules must never depend on the GUI toolkit.
//!
//! Keeping those layers GTK-free is what makes the domain logic and the config
//! parsers headlessly testable (R6.2). Because Settings4000 is a single crate
//! (a library plus a thin binary, rather than a workspace with a separate GUI
//! crate), the compiler does not enforce this for us — so this test scans those two modules
//! and fails if any file imports or otherwise references `gtk`, `gtk4`, or
//! `relm4` (the latter re-exports GTK and would be an equivalent backdoor).
//!
//! It also guards a small set of **individual files inside the otherwise-GTK
//! `ui/` layer** that are GTK-free by design — currently `src/ui/startup.rs`,
//! the worker-thread startup-load logic (task 5.4), which is headlessly tested
//! (R6.2) and must stay pure so the load can be reasoned about without a
//! display. Nothing else prevents a future edit from importing the toolkit
//! there, so this test does. For those files the forbidden set additionally
//! includes `glib` (the GLib bindings `gtk4` re-exports): the threading handoff
//! belongs in `window.rs`, which owns the persistent shell, not in the pure
//! load logic.
//!
//! The task breakdown (`docs/tasks.md` §1.1) explicitly calls for this
//! grep-style guard "or workspace crate split"; should the crate later be split
//! so that `core`/`parsers` live in a GUI-free crate, this test becomes
//! redundant and can be removed.
//!
//! # What this guard does and does not promise
//!
//! It is a lexical scanner, not a compiler, so its promise is narrow: **no line
//! of the guarded files names a forbidden crate, either as an import target or
//! as the root of a path.** The ordinary ways of reaching for the toolkit are
//! all within that promise — `use gtk4::…`, `pub use relm4::…`,
//! `extern crate gtk`, a bare `gtk4::Window::new()`, the globally-qualified
//! `::gtk4::Window::new()`, a raw identifier `r#gtk4::…` — and so is a reference
//! that shares a line with a string literal, because `lexical_guard`'s scanner
//! understands comments *and* literals (a `//` or `/*` inside a string used to
//! blank the code around it, hiding real references; see that module).
//!
//! Spellings the scanner is known **not** to catch, so that nobody mistakes its
//! green result for a proof:
//!
//! - **A path split across lines** — `gtk4` at the end of one line and
//!   `::Window` at the start of the next. The scan is line-by-line.
//! - **Whitespace inside the path** — `gtk4 :: Window`. This one is covered by a
//!   different CI gate rather than by this test: `cargo fmt --check` is
//!   mandatory and rustfmt rewrites such a path to `gtk4::Window`, which this
//!   scanner then sees.
//! - **Any indirect route that never names the crate** — a glob import from a
//!   module that does depend on the toolkit (`use crate::ui::something::*;`
//!   followed by a bare `Window::new()`), a macro expanding to a GTK path, a
//!   `#[path = "…"]` attribute pulling a GTK source file into these modules, or
//!   a dependency renamed in `Cargo.toml` (`toolkit = { package = "gtk4" }`,
//!   then `use toolkit::…`) — that last one is not even in the scanned tree.
//!
//! Closing that last class needs the compiler, i.e. the "workspace crate split"
//! that `docs/tasks.md` §1.1 offers as the alternative to this guard. Until then
//! they are accepted risks: each requires a deliberate and conspicuous edit.
//! Note also that the list above is of *known* gaps, not a proof that no others
//! exist — a lexical scanner cannot promise that. What it does promise is that a
//! violation it fails to catch had to be written in an unusual way; anything
//! written the usual way fails this test.

use std::fs;
use std::path::{Path, PathBuf};

// The `.rs` walker and the code/non-code splitter are shared with the other
// lexical source-policy guard (`tests/no_custom_css.rs`); see that module for why
// the sharing works this way.
mod lexical_guard;

use lexical_guard::{rust_sources, strip_comments};

/// Crate roots that must not appear in an import within `core/` or `parsers/`.
///
/// `gtk`/`gtk4` are the bindings themselves; `relm4` is included because it
/// re-exports `gtk`, so importing it would smuggle the toolkit into the
/// supposedly headless layers just as effectively.
const FORBIDDEN_CRATES: &[&str] = &["gtk", "gtk4", "relm4"];

/// Crate roots forbidden in the individually-guarded GTK-free files under `ui/`
/// (see [`GTK_FREE_UI_FILES`]).
///
/// Extends [`FORBIDDEN_CRATES`] with `glib`, the GLib bindings `gtk4` re-exports:
/// `src/ui/startup.rs` is the worker-thread startup-load logic and is GTK-free by
/// construction (headlessly tested, R6.2), so it must not reach for glib's
/// main-context/executor either — the threading handoff belongs in `window.rs`,
/// which owns the persistent shell.
const UI_FILE_FORBIDDEN_CRATES: &[&str] = &["gtk", "gtk4", "relm4", "glib"];

/// The GTK-free layers whose source is scanned. Paths are relative to the crate
/// root (`CARGO_MANIFEST_DIR`).
const GTK_FREE_LAYERS: &[&str] = &["src/core", "src/parsers"];

/// GTK-free source *files* that live inside the otherwise-GTK `ui/` layer and so
/// are guarded individually rather than by directory. Paths are relative to the
/// crate root.
const GTK_FREE_UI_FILES: &[&str] = &["src/ui/startup.rs"];

#[test]
fn core_and_parsers_are_gtk_free() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let mut files: Vec<PathBuf> = Vec::new();
    for layer in GTK_FREE_LAYERS {
        let layer_dir = manifest_dir.join(layer);
        assert!(
            layer_dir.is_dir(),
            "expected GTK-free layer directory {} to exist (architecture §2)",
            layer_dir.display()
        );
        files.extend(rust_sources(&layer_dir));
    }

    let violations = scan_for_forbidden(&files, FORBIDDEN_CRATES, &manifest_dir);
    assert!(
        violations.is_empty(),
        "`core/` and `parsers/` must not import or reference gtk/relm4 \
         (architecture §2, R6.2). Offending lines:\n{}",
        violations.join("\n")
    );
}

#[test]
fn startup_load_logic_is_gtk_free() {
    // `src/ui/startup.rs` lives under the GTK `ui/` layer but is GTK-free by
    // design (task 5.4): its detection + config-parsing logic is headlessly
    // tested (R6.2), so it must not import the toolkit — nor `glib`, since the
    // worker/main-thread handoff belongs in `window.rs`, not the pure load.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let files: Vec<PathBuf> = GTK_FREE_UI_FILES
        .iter()
        .map(|file| manifest_dir.join(file))
        .collect();
    for file in &files {
        assert!(
            file.is_file(),
            "expected guarded GTK-free file {} to exist (task 5.4)",
            file.display()
        );
    }

    let violations = scan_for_forbidden(&files, UI_FILE_FORBIDDEN_CRATES, &manifest_dir);
    assert!(
        violations.is_empty(),
        "the GTK-free `ui/` files must not import or reference gtk/relm4/glib \
         (architecture §2, R6.2). Offending lines:\n{}",
        violations.join("\n")
    );
}

/// Scans each file in `files` for a reference to any crate in `forbidden`,
/// returning a repo-relative description of every offending line.
///
/// Comments and literals are blanked first (see [`strip_comments`]) so a crate
/// name mentioned in prose or in a string never counts — and, conversely, so no
/// string can blank the code around it. Shared by both guards so the layer scan
/// and the individual-file scan apply identical lexical rules, differing only in
/// which crates they forbid.
fn scan_for_forbidden(
    files: &[PathBuf],
    forbidden: &[&'static str],
    manifest_dir: &Path,
) -> Vec<String> {
    let mut violations: Vec<String> = Vec::new();
    for file in files {
        let source = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        let code = strip_comments(&source);

        for (line_no, line) in code.lines().enumerate() {
            if let Some(crate_name) = forbidden_reference(line, forbidden) {
                // Report a repo-relative path so the failure reads clearly
                // regardless of where the checkout lives.
                let rel = file.strip_prefix(manifest_dir).unwrap_or(file);
                violations.push(format!(
                    "{}:{} references `{crate_name}`: {}",
                    rel.display(),
                    line_no + 1,
                    line.trim()
                ));
            }
        }
    }
    violations
}

/// Inspects a single comment-free code line and returns the forbidden crate it
/// references, if any, from the `forbidden` set.
///
/// Two shapes are recognized:
/// 1. An import declaration — `use <crate>…` or `extern crate <crate>` (after
///    an optional `pub`/`pub(…)` visibility modifier).
/// 2. A path rooted at the crate — `<crate>::…` or the globally-qualified
///    `::<crate>::…`, anywhere on the line, used without any `use` (see
///    [`is_path_root`] for how the root is distinguished from an inner segment).
///
/// The `forbidden` set is a parameter so the same lexical rules guard both the
/// `core`/`parsers` layers and the individually-guarded `ui/` files, which forbid
/// a slightly wider set (adding `glib`).
fn forbidden_reference(line: &str, forbidden: &[&'static str]) -> Option<&'static str> {
    let trimmed = strip_visibility(line.trim());

    // Case 1: import declarations.
    let import_target = trimmed
        .strip_prefix("use ")
        .or_else(|| trimmed.strip_prefix("extern crate "));
    if let Some(rest) = import_target {
        let first = first_path_segment(rest);
        if let Some(&crate_name) = forbidden.iter().find(|&&c| first == c) {
            return Some(crate_name);
        }
    }

    // Case 2: any fully-qualified `<crate>::` usage anywhere on the line.
    forbidden
        .iter()
        .find(|&&crate_name| contains_path_prefix(line, crate_name))
        .copied()
}

/// Strips a leading visibility modifier (`pub`, `pub(crate)`, `pub(super)`,
/// `pub(in path)`) so that `pub use relm4::…` is recognized like a plain
/// `use relm4::…`.
fn strip_visibility(line: &str) -> &str {
    let Some(after_pub) = line.strip_prefix("pub") else {
        return line;
    };
    let after_pub = after_pub.trim_start();
    // `pub(...)` restricted visibility: skip the parenthesized part.
    if let Some(stripped) = after_pub.strip_prefix('(') {
        if let Some(close) = stripped.find(')') {
            return stripped[close + 1..].trim_start();
        }
    }
    after_pub
}

/// Extracts the first `::`-delimited identifier from a path, ignoring a leading
/// `::` (as in `use ::gtk::…`) and a raw-identifier marker (`use r#gtk4::…`,
/// which is legal because `gtk4` is not a keyword). For `gtk4::prelude::*;` this
/// yields `gtk4`.
fn first_path_segment(path: &str) -> String {
    path.trim_start()
        .trim_start_matches("::")
        .trim_start_matches("r#")
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// Reports whether `line` references `<crate>` as the *root* of a
/// fully-qualified path — the crate name immediately followed by `::`, standing
/// on its own rather than as a segment of a longer path or as the tail of a
/// longer identifier.
///
/// Every occurrence on the line is examined, because one rejected occurrence
/// says nothing about the next (`crate::theme::gtk::Model` and a real
/// `gtk4::Window` can share a line).
fn contains_path_prefix(line: &str, crate_name: &str) -> bool {
    let needle = format!("{crate_name}::");
    let mut search_from = 0;
    while let Some(pos) = line[search_from..].find(&needle) {
        let abs = search_from + pos;
        if is_path_root(&line[..abs]) {
            return true;
        }
        search_from = abs + needle.len();
    }
    false
}

/// Decides whether a `<crate>::` occurrence stands as the *root* of a path,
/// judging by `before` — the code preceding it on the same line.
///
/// The cases, in the order they are tested:
///
/// 1. **Nothing precedes it.** A root (the line starts with the crate name).
/// 2. **An identifier character** (alphanumeric or `_`). Not a root, but the
///    tail of a longer name such as `nix_gtk::` — a different crate.
/// 3. **A `::` qualifier.** A root only when the `::` is *not* itself preceded
///    by an identifier character:
///    - `::gtk4::Window::new()` — a **global path**. It needs no `use` at all
///      (the crate is a direct dependency), so it is a genuine toolkit
///      reference and must be flagged. Missing this spelling was the hole task
///      9.13 closed.
///    - `crate::core::theme::gtk::Model` — an inner segment of a longer path,
///      i.e. an intra-crate module or type merely *named* after the toolkit.
///      That is plausible here, since the app legitimately edits GTK themes
///      (task 6.4), so it must not be flagged.
/// 4. **Anything else.** A root. This covers whitespace and delimiters
///    (`(`, `<`, `&`, `,`) as well as a lone `:`, which introduces a type in an
///    ascription or bound (`let w:gtk4::Window`) and is therefore a real
///    reference.
///
/// The rule is intentionally asymmetric: the only case it rejects outright is a
/// path that provably continues to the left. An exotic prefix it has not
/// anticipated is flagged rather than waved through, so an unforeseen spelling
/// costs a false positive instead of a silent miss.
fn is_path_root(before: &str) -> bool {
    let mut preceding = before.chars().rev();

    let Some(prev) = preceding.next() else {
        return true; // Case 1: start of line.
    };
    if is_ident_char(prev) {
        return false; // Case 2: tail of a longer identifier.
    }
    if prev != ':' {
        return true; // Case 4: whitespace, a delimiter, or a lone `:`.
    }

    // A preceding `:` is the second colon of a `::` when another `:` sits
    // before it; a single `:` is a type ascription and falls into case 4.
    match preceding.next() {
        // Case 3: `::<crate>::`. A global path unless the `::` continues a
        // longer path to its left.
        Some(':') => !preceding.next().is_some_and(is_ident_char),
        _ => true,
    }
}

/// Reports whether `c` may appear inside a Rust identifier.
///
/// Used for the path-boundary checks. Non-ASCII bytes have already been blanked
/// by `lexical_guard::strip_comments`, so in practice this only ever sees ASCII.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Asserts that `line` is reported as a reference to `expected`.
///
/// The lines in the tests below are hand-written samples of *legal Rust* that a
/// future edit to `core/`/`parsers/` could plausibly contain; the point of the
/// samples is that the guard's verdict on each spelling is pinned, so nobody has
/// to re-derive the boundary rules from the code to know what is covered.
#[track_caller]
fn assert_flagged(line: &str, expected: &str) {
    assert_eq!(
        forbidden_reference(line, FORBIDDEN_CRATES),
        Some(expected),
        "expected {line:?} to be flagged as a reference to `{expected}`"
    );
}

/// Asserts that `line` is *not* reported, because it is legitimate code that a
/// reader might expect to trip the guard.
#[track_caller]
fn assert_not_flagged(line: &str) {
    assert_eq!(
        forbidden_reference(line, FORBIDDEN_CRATES),
        None,
        "expected {line:?} not to be flagged"
    );
}

/// Asserts that `line` is *not* reported because it is one of the blind spots
/// listed in this file's module docs.
///
/// Separate from [`assert_not_flagged`] purely for its failure message: this
/// assertion fails when someone *improves* the scanner, and the message a
/// contributor reads in CI has to say so — otherwise it looks like the change
/// broke something and invites reverting a genuine hardening.
#[track_caller]
fn assert_documented_blind_spot(line: &str) {
    assert_eq!(
        forbidden_reference(line, FORBIDDEN_CRATES),
        None,
        "{line:?} is recorded in this file's module docs as a spelling the scanner \
         cannot see, but it was just caught. If you hardened the scanner, nothing is \
         broken — that is an improvement: move this case to the catch-list tests and \
         delete its entry from the module docs' blind-spot list. Only revert if the \
         match was accidental."
    );
}

#[test]
fn import_declarations_are_flagged_in_every_visibility_and_alias_form() {
    assert_flagged("use gtk4::prelude::*;", "gtk4");
    assert_flagged("use gtk::Widget;", "gtk");
    assert_flagged("pub use relm4::Component;", "relm4");
    assert_flagged("pub(crate) use gtk4::Window;", "gtk4");
    assert_flagged("pub(in crate::core) use gtk4::Window;", "gtk4");
    assert_flagged("extern crate gtk4;", "gtk4");
    // A global-path import, and an alias that hides the crate name from the
    // rest of the file.
    assert_flagged("use ::gtk4::Window;", "gtk4");
    assert_flagged("use gtk4::Window as W;", "gtk4");
    assert_flagged("use relm4 as framework;", "relm4");
    // A raw identifier is legal here (`gtk4` is not a keyword) and would
    // otherwise read as a crate named `r`.
    assert_flagged("use r#gtk4::Label;", "gtk4");
}

#[test]
fn paths_rooted_at_a_forbidden_crate_are_flagged_without_any_import() {
    assert_flagged("    let w = gtk4::Window::new();", "gtk4");
    // The globally-qualified spelling: legal with no `use` at all, because the
    // crate is a direct dependency. Missing it was the hole task 9.13 closed.
    assert_flagged("    let w = ::gtk4::Window::new();", "gtk4");
    assert_flagged("    <gtk4::Window as Default>::default();", "gtk4");
    assert_flagged("    let v = Vec::<gtk4::Widget>::new();", "gtk4");
    assert_flagged("    let v = Vec::<::gtk4::Widget>::new();", "gtk4");
    assert_flagged("    let w: gtk4::Window = todo!();", "gtk4");
    // A type ascription written without the customary space: the `:` here is a
    // lone colon, not half of a `::`.
    assert_flagged("    let w:gtk4::Window = todo!();", "gtk4");
    assert_flagged("    gtk4::glib::clone!(@strong x => move |_| {});", "gtk4");
    assert_flagged("    fn send(tx: &::relm4::Sender<u8>) {}", "relm4");
    assert_flagged(
        "    fn bound<T: gtk4::prelude::IsA<gtk4::Widget>>() {}",
        "gtk4",
    );
}

#[test]
fn intra_crate_paths_and_lookalike_names_are_not_flagged() {
    // An inner path segment merely *named* after the toolkit. The app edits GTK
    // themes (task 6.4), so a GTK-free `core::…::gtk` module is plausible.
    assert_not_flagged("use crate::core::theme::gtk::Model;");
    assert_not_flagged("    let m = crate::core::theme::gtk::Model::new();");
    assert_not_flagged("    let m = self::gtk::helper();");
    // Different crates whose names merely end in a forbidden one.
    assert_not_flagged("use nix_gtk::Widget;");
    assert_not_flagged("    let x = my_relm4::thing();");
    // Ordinary identifiers containing a forbidden name but no path root.
    assert_not_flagged("    let gtk_theme_name = read_key(\"gtk-theme-name\")?;");
    assert_not_flagged("use crate::parsers::ini::KEY_GTK_THEME;");
}

#[test]
fn a_rejected_occurrence_does_not_mask_a_real_one_later_on_the_line() {
    // The first `gtk::` is an inner segment; the second is a crate root. A
    // scanner that stopped at the first rejection would miss the real one.
    assert_flagged(
        "    let m = crate::core::theme::gtk::Model::from(gtk::Align::Fill);",
        "gtk",
    );
}

#[test]
fn glib_is_forbidden_only_in_the_individually_guarded_ui_files() {
    // `src/ui/startup.rs` must not reach for glib's main context either (the
    // worker/main-thread handoff belongs in `window.rs`), while the rest of the
    // GTK-free layers are scanned with the narrower set.
    let line = "    let ctx = glib::MainContext::default();";
    assert_eq!(
        forbidden_reference(line, UI_FILE_FORBIDDEN_CRATES),
        Some("glib")
    );
    assert_eq!(forbidden_reference(line, FORBIDDEN_CRATES), None);
}

#[test]
fn a_reference_sharing_a_line_with_a_string_literal_is_still_flagged() {
    // Whole-file scanning strips comments *and* literals before matching, so a
    // string is no longer able to blank the code beside it. Both lines below are
    // legal Rust that a config-editing app could plausibly contain, and both hid a
    // real toolkit reference from the earlier, string-blind scanner: the `//` in a
    // URL was read as a line comment, and the `/*` in a glob pattern opened a
    // block comment that ran on into the following lines. `lexical_guard` has the
    // multi-line half of this; here the guard's own verdict is pinned.
    let line = strip_comments(
        "    let ok = matches!((s == \"https://a\", ::gtk4::Align::Fill), (true, _));",
    );
    assert_flagged(&line, "gtk4");

    let glob = strip_comments("    let p = \"/*.conf\"; let a = gtk4::Align::Fill;");
    assert_flagged(&glob, "gtk4");

    // The other direction, unchanged: a crate name that is only ever *mentioned*
    // in a string is not a use of it, and no longer reported.
    let mention = strip_comments("    const DOC: &str = \"see gtk4::Widget\";");
    assert_not_flagged(&mention);
}

#[test]
fn the_documented_blind_spots_are_still_blind_spots() {
    // This test pins the *limits* stated in this file's module docs, so that
    // hardening the scanner fails here and the docs get corrected with it. See
    // `assert_documented_blind_spot` for why it has its own assertion.
    //
    // Whitespace inside a path. `cargo fmt --check` is a mandatory CI gate and
    // rustfmt rewrites this to `gtk4::Window`, which the scanner does catch, so
    // the spelling cannot actually land in the repo.
    assert_documented_blind_spot("    let w = gtk4 :: Window::new();");
    // A path split across lines: the scan is line-by-line, so neither half
    // carries a `<crate>::` token.
    assert_documented_blind_spot("    let w = gtk4");
    assert_documented_blind_spot("        ::Window::new();");
    // Indirect routes that never name the crate. Only a compiler-enforced crate
    // split (`docs/tasks.md` §1.1) could catch these.
    assert_documented_blind_spot("use crate::ui::reexports::*;");
    assert_documented_blind_spot("    let w = Window::new();");
    // A dependency renamed in `Cargo.toml` (`toolkit = { package = "gtk4" }`):
    // the deception is not even in the scanned tree.
    assert_documented_blind_spot("use toolkit::Window;");
}
