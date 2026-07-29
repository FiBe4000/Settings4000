//! Enforces the styling rule from `docs/requirements.md` R2.1: Settings4000 ships
//! **no custom CSS** and injects **no palette**, so it renders entirely with the
//! active system GTK theme and matches the rest of the desktop.
//!
//! GTK4 lets an app override the theme by loading its own CSS through a
//! `gtk4::CssProvider` and installing it on a display or a widget's style context
//! (`add_provider` / `add_provider_for_display`). Doing any of that would defeat
//! R2.1 (the app would stop matching the system theme) and, combined with
//! libadwaita, is exactly the trap the architecture calls out (§7). Because the
//! compiler cannot forbid an API by policy, this test scans the source tree and
//! fails if any file reaches for a custom-CSS mechanism — so a future change cannot
//! silently reintroduce styling. It mirrors `tests/module_boundaries.rs`, which
//! guards the GTK-free layering rule the same way.
//!
//! This is a lexical scanner, not a compiler, and what it promises is
//! correspondingly narrow: **no line under `src/` spells one of the listed API
//! names in code.** Comments and string/character literals are blanked first (see
//! `lexical_guard::strip_comments`), which serves the rule in both directions: the
//! policy can be *documented* in rustdoc — `ui/theme.rs` and `ui/window.rs` both
//! explain why a `CssProvider` is forbidden — and, less obviously, a `//` or `/*`
//! inside an ordinary string can no longer blank the code around it and hide a
//! real call. (`ui/sound.rs` contains a `"/*"` string that used to open a phantom
//! block comment, blanking the code beside it into the following line until a
//! later `*/` closed it; a `"//"` string further down truncated its own line.)
//!
//! Beyond that promise the scanner cannot see: a route that never spells one of
//! the names — a macro expanding to a `CssProvider`, or CSS loaded by a
//! dependency — is out of reach. The listed names are simply the only way to
//! inject custom CSS that this codebase could plausibly grow.

use std::fs;
use std::path::PathBuf;

// The `.rs` walker and the code/non-code splitter are shared with the other
// lexical source-policy guard (`tests/module_boundaries.rs`); see that module for
// why the sharing works this way.
mod lexical_guard;

use lexical_guard::{rust_sources, strip_comments};

/// Custom-CSS APIs that must not appear anywhere in `src/`.
///
/// Injecting custom CSS in GTK4 requires a `CssProvider` (loaded from a string, a
/// file, or a resource) that is then installed via `add_provider` /
/// `add_provider_for_display` (including the deprecated `StyleContext::add_provider`
/// path). Catching the provider type and the install call covers the whole flow;
/// `load_from_data` / `load_from_string` are listed explicitly because they are the
/// string-injection entry points R2.1 most directly forbids. Using a system
/// theme-defined style class (`add_css_class`) is *not* custom CSS and is not listed:
/// it selects styling the active theme already provides rather than shipping our own.
const FORBIDDEN_CSS_APIS: &[&str] = &[
    "CssProvider",
    "load_from_data",
    "load_from_string",
    "add_provider",
];

#[test]
fn src_uses_no_custom_css() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");
    assert!(
        src_dir.is_dir(),
        "expected the source directory {} to exist",
        src_dir.display()
    );

    let mut violations: Vec<String> = Vec::new();

    for file in rust_sources(&src_dir) {
        let source = fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        let code = strip_comments(&source);

        for (line_no, line) in code.lines().enumerate() {
            for api in FORBIDDEN_CSS_APIS {
                if line.contains(api) {
                    // Report a repo-relative path so the failure reads clearly
                    // regardless of where the checkout lives.
                    let rel = file.strip_prefix(&manifest_dir).unwrap_or(&file);
                    violations.push(format!(
                        "{}:{} references custom-CSS API `{api}`: {}",
                        rel.display(),
                        line_no + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "src/ must not use any custom-CSS API — the app inherits the system GTK theme \
         (R2.1). Offending lines:\n{}",
        violations.join("\n")
    );
}
