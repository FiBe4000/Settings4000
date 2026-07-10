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
//! This is a lexical scanner, not a compiler. It blanks comments (see
//! `strip_comments`) so the rule can be *documented* in rustdoc without tripping the
//! guard, but it does not parse string literals; a forbidden name inside a string
//! constant would be reported. That is a deliberate, low-risk simplification: no
//! such literal exists in this codebase, and if one were ever added the failure is a
//! loud false positive a human resolves, never a real CSS override slipping through.

use std::fs;
use std::path::{Path, PathBuf};

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

/// Collects every `.rs` file under `dir`, recursing into subdirectories so nested
/// module files (e.g. `ui/window.rs`) are covered too.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read directory {}: {e}", dir.display()));

    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("failed to read entry in {}: {e}", dir.display()))
            .path();

        if path.is_dir() {
            files.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }

    files
}

/// Returns a copy of `source` with Rust comments blanked out so that a mention of a
/// forbidden API inside a doc comment (e.g. documenting *why* we avoid it) never
/// counts as a real use.
///
/// Block comments (`/* … */`) are removed first, then line comments (`//`, `///`,
/// `//!`). Removed spans are replaced with spaces rather than deleted so that
/// byte/line positions are preserved for readable failure messages. This is a
/// pragmatic scanner, not a full Rust lexer: it does not special-case `//` or `/*`
/// occurring inside string literals, which do not appear in these files.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    while i < bytes.len() {
        // Start of a block comment: consume through the matching `*/`, emitting a
        // space for every consumed byte except newlines (kept so line numbering
        // stays intact).
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            out.push_str("  ");
            while i < bytes.len()
                && !(bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/')
            {
                out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                i += 1;
            }
            if i < bytes.len() {
                i += 2; // skip the closing `*/`
                out.push_str("  ");
            }
            continue;
        }

        // Start of a line comment (covers `//`, `///`, `//!`): drop the rest of the
        // line but keep the newline.
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Emit ASCII bytes verbatim; replace any non-ASCII byte with a space. The
        // tokens we scan for are pure ASCII, so this can neither create nor destroy a
        // real match.
        out.push(if bytes[i] < 0x80 {
            bytes[i] as char
        } else {
            ' '
        });
        i += 1;
    }

    out
}
