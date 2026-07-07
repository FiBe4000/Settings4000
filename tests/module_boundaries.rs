//! Enforces the hard layering rule from `docs/architecture.md` §2: the
//! `core/` and `parsers/` modules must never depend on the GUI toolkit.
//!
//! Keeping those layers GTK-free is what makes the domain logic and the config
//! parsers headlessly testable (R6.2). Because Settings4000 is a single binary
//! crate (rather than a workspace with a separate GUI crate), the compiler does
//! not enforce this for us — so this test scans the source of those two modules
//! and fails if any file imports or otherwise references `gtk`, `gtk4`, or
//! `relm4` (the latter re-exports GTK and would be an equivalent backdoor).
//!
//! The task breakdown (`docs/tasks.md` §1.1) explicitly calls for this
//! grep-style guard "or workspace crate split"; should the crate later be split
//! so that `core`/`parsers` live in a GUI-free crate, this test becomes
//! redundant and can be removed.
//!
//! This is a lexical scanner, not a compiler. It blanks comments (see
//! `strip_comments`) but does not parse string literals, so a forbidden crate
//! name embedded in a string constant (e.g. `const S: &str = "gtk4::Widget";`)
//! would be reported as a violation. That is a deliberate, low-risk
//! simplification: such literals do not occur in these headless modules, and if
//! one ever did the failure is a loud false positive that a human resolves —
//! never a real import slipping through undetected.

use std::fs;
use std::path::{Path, PathBuf};

/// Crate roots that must not appear in an import within `core/` or `parsers/`.
///
/// `gtk`/`gtk4` are the bindings themselves; `relm4` is included because it
/// re-exports `gtk`, so importing it would smuggle the toolkit into the
/// supposedly headless layers just as effectively.
const FORBIDDEN_CRATES: &[&str] = &["gtk", "gtk4", "relm4"];

/// The GTK-free layers whose source is scanned. Paths are relative to the crate
/// root (`CARGO_MANIFEST_DIR`).
const GTK_FREE_LAYERS: &[&str] = &["src/core", "src/parsers"];

#[test]
fn core_and_parsers_are_gtk_free() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let mut violations: Vec<String> = Vec::new();

    for layer in GTK_FREE_LAYERS {
        let layer_dir = manifest_dir.join(layer);
        assert!(
            layer_dir.is_dir(),
            "expected GTK-free layer directory {} to exist (architecture §2)",
            layer_dir.display()
        );

        for file in rust_sources(&layer_dir) {
            let source = fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
            let code = strip_comments(&source);

            for (line_no, line) in code.lines().enumerate() {
                if let Some(crate_name) = forbidden_reference(line) {
                    // Report a repo-relative path so the failure reads clearly
                    // regardless of where the checkout lives.
                    let rel = file.strip_prefix(&manifest_dir).unwrap_or(&file);
                    violations.push(format!(
                        "{}:{} references `{crate_name}`: {}",
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
        "`core/` and `parsers/` must not import or reference gtk/relm4 \
         (architecture §2, R6.2). Offending lines:\n{}",
        violations.join("\n")
    );
}

/// Collects every `.rs` file under `dir`, recursing into subdirectories so that
/// nested module files (e.g. `core/detect/mod.rs`) are covered too.
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

/// Returns a copy of `source` with Rust comments blanked out so that a `gtk`
/// mention inside a doc comment or example never counts as a real dependency.
///
/// Block comments (`/* … */`) are removed first, then line comments (`//` and
/// doc `///`). Removed spans are replaced with spaces rather than deleted so
/// that byte/line positions are preserved for readable failure messages. This
/// is a pragmatic scanner, not a full Rust lexer: it does not special-case
/// `//` or `/*` occurring inside string literals, which do not appear in these
/// modules' import statements and are vanishingly unlikely elsewhere in them.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    while i < bytes.len() {
        // Start of a block comment: consume through the matching `*/`,
        // emitting a space for every consumed byte except newlines (kept so
        // line numbering stays intact).
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

        // Start of a line comment (covers `//`, `///`, `//!`): drop the rest
        // of the line but keep the newline.
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Emit ASCII bytes verbatim; replace any non-ASCII byte (a UTF-8
        // continuation/lead byte, always >= 0x80) with a space. The tokens we
        // scan for (`use`, `::`, the crate names) are pure ASCII, so this can
        // neither create nor destroy a real match.
        out.push(if bytes[i] < 0x80 {
            bytes[i] as char
        } else {
            ' '
        });
        i += 1;
    }

    out
}

/// Inspects a single comment-free code line and returns the forbidden crate it
/// references, if any.
///
/// Two shapes are recognized:
/// 1. An import declaration — `use <crate>…` or `extern crate <crate>` (after
///    an optional `pub`/`pub(…)` visibility modifier).
/// 2. A fully-qualified path — a bare `<crate>::` prefix used without a `use`.
fn forbidden_reference(line: &str) -> Option<&'static str> {
    let trimmed = strip_visibility(line.trim());

    // Case 1: import declarations.
    let import_target = trimmed
        .strip_prefix("use ")
        .or_else(|| trimmed.strip_prefix("extern crate "));
    if let Some(rest) = import_target {
        let first = first_path_segment(rest);
        if let Some(&crate_name) = FORBIDDEN_CRATES.iter().find(|&&c| first == c) {
            return Some(crate_name);
        }
    }

    // Case 2: any fully-qualified `<crate>::` usage anywhere on the line.
    FORBIDDEN_CRATES
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
/// `::` (as in `use ::gtk::…`). For `gtk4::prelude::*;` this yields `gtk4`.
fn first_path_segment(path: &str) -> String {
    path.trim_start()
        .trim_start_matches("::")
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// Reports whether `line` references `<crate>` as the *root* of a
/// fully-qualified path — the crate name immediately followed by `::`, standing
/// on its own rather than as a segment of a longer path or identifier.
///
/// The left-boundary guard rejects two kinds of preceding character:
/// - an identifier character (alphanumeric or `_`), so a longer name that
///   merely ends in the crate name does not match (e.g. `nix_gtk::`);
/// - a `:`, so an *inner* path segment does not match. A preceding `:` is the
///   second colon of a `::`, meaning the crate name sits mid-path
///   (e.g. `crate::core::theme::gtk::Model` or `cfg::gtk4::Value`). Such a
///   segment is an intra-crate module or type merely named after the toolkit,
///   not an import of it — and the app legitimately edits GTK themes
///   (task 6.4), so a GTK-free `core::…::gtk` submodule is plausible and must
///   not be flagged as a violation.
///
/// Genuine toolkit references are still caught: a crate-root use such as
/// `gtk4::Window::new()` is preceded by whitespace or a delimiter, and actual
/// imports (`use gtk4::…`, `use ::gtk4::…`, `extern crate gtk4`) are matched by
/// the import-declaration case in `forbidden_reference` before this runs, so
/// rejecting a leading `:` here loses no real coverage.
fn contains_path_prefix(line: &str, crate_name: &str) -> bool {
    let needle = format!("{crate_name}::");
    let mut search_from = 0;
    while let Some(pos) = line[search_from..].find(&needle) {
        let abs = search_from + pos;
        // Guard the left boundary. The crate name counts as a path root only
        // when the preceding character is neither an identifier character (else
        // it is the tail of a longer name such as `nix_gtk::`) nor a `:` (else
        // it is an inner segment of a longer path such as `crate::…::gtk::`,
        // not a crate-root reference). See this function's doc for why the
        // inner-segment case must not be flagged.
        let boundary_ok = abs == 0
            || !line[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':');
        if boundary_ok {
            return true;
        }
        search_from = abs + needle.len();
    }
    false
}
