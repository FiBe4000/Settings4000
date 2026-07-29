//! Shared lexical-scanning primitives for the source-policy guards in `tests/`.
//!
//! Two policies in this project cannot be expressed to the compiler and are
//! therefore enforced by scanning the source tree instead:
//!
//! - `tests/module_boundaries.rs` — `core/` and `parsers/` must never reference
//!   the GUI toolkit (`docs/architecture.md` §2, R6.2).
//! - `tests/no_custom_css.rs` — no file may reach for a custom-CSS API, so the
//!   app renders with the active system GTK theme (R2.1).
//!
//! Both need the same two primitives — walk a directory for `.rs` files, and
//! blank out everything that is not code (comments, string and character
//! literals) so the policy can be *documented* in rustdoc without tripping the
//! guard that enforces it — which is why they live here instead of being
//! copy-pasted into each guard (task 9.13). Each guard is a separate
//! integration-test crate, so sharing happens by declaring `mod lexical_guard;`
//! in both crate roots. The module deliberately sits in a subdirectory: a
//! top-level `tests/*.rs` file would be compiled as a test target of its own.
//!
//! Because both crates compile this module, the unit tests below run once per
//! guard — the same test name appears in both targets' output. That is
//! harmless, and keeping the tests next to the code they cover is worth it: a
//! regression in the lexer now silently weakens two guards at once.

use std::fs;
use std::path::{Path, PathBuf};

/// Collects every `.rs` file under `dir`, recursing into subdirectories so that
/// nested module files (e.g. `core/detect/mod.rs`) are covered too.
///
/// # Panics
///
/// Panics on any I/O failure. A guard that cannot read the tree it is supposed
/// to police must fail loudly rather than silently scan nothing — an empty scan
/// would report zero violations and look like a pass.
pub fn rust_sources(dir: &Path) -> Vec<PathBuf> {
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

/// Returns a copy of `source` with everything that is not code blanked out —
/// comments, string literals and character literals — so that the guards scan
/// code and nothing else.
///
/// # Why each of the three is blanked
///
/// - **Comments**, so that a policy can be *documented* in rustdoc — naming the
///   very crate or API it forbids — without tripping the guard that enforces it.
/// - **String literals**, for two reasons. The mild one is a false positive: a
///   forbidden name inside a string (`const S: &str = "gtk4::Widget";`) is not a
///   use of it. The serious one is a *silent miss*. A scanner that does not know
///   about strings reads the `//` in `"https://example.com"` as starting a line
///   comment and blanks the rest of that line, and reads the `/*` in a glob such
///   as `"/*.conf"` as opening a block comment that swallows **every following
///   line** until some later `*/` closes it. Either one can hide a real
///   violation, with no warning — the one failure mode a guard must not have.
///   (Both were demonstrated against the earlier version of this lexer; the
///   tests below reproduce them.)
/// - **Character literals**, because `'"'` and `b'"'` would otherwise open a
///   phantom string that runs to the next quote — `src/parsers/swaync.rs` has
///   two such literals four lines apart.
///
/// Blanked spans become spaces, with newlines preserved, rather than being
/// deleted: byte and line positions survive, so the guards can quote the
/// offending line number. Non-ASCII bytes are replaced with spaces for the same
/// reason — every token the guards search for is pure ASCII, so this can neither
/// create nor destroy a real match, and it keeps the scan byte-indexable.
///
/// # What this still is not
///
/// A real Rust lexer, let alone a parser. It recognizes what actually occurs in
/// this codebase: comments (nested block comments included, as Rust nests them),
/// the four string spellings (`"…"`, `b"…"`, `r#"…"#`, `br#"…"#`, with `\`
/// escapes in the non-raw forms) and character literals. Above the lexical level
/// it knows nothing: code produced by a macro, or pulled in by `include!`, is
/// invisible to it. Each guard documents what that leaves open for the policy it
/// enforces.
pub fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    while i < bytes.len() {
        // Each of these returns the index just past the construct it matched, or
        // `None` when no such construct starts here. Order does not matter: the
        // openers are mutually exclusive.
        let skip_to = block_comment_end(bytes, i)
            .or_else(|| line_comment_end(bytes, i))
            .or_else(|| string_literal_end(bytes, i))
            .or_else(|| char_literal_end(bytes, i));

        if let Some(end) = skip_to {
            blank(&mut out, &bytes[i..end]);
            i = end;
            continue;
        }

        // Ordinary code: emit ASCII verbatim, and replace any non-ASCII byte (a
        // UTF-8 lead or continuation byte, always >= 0x80) with a space.
        out.push(if bytes[i] < 0x80 {
            bytes[i] as char
        } else {
            ' '
        });
        i += 1;
    }

    out
}

/// Appends `span` to `out` as spaces, keeping its newlines so that line
/// numbering — which the guards report — is unaffected by what was blanked.
fn blank(out: &mut String, span: &[u8]) {
    for &byte in span {
        out.push(if byte == b'\n' { '\n' } else { ' ' });
    }
}

/// If a block comment starts at `start`, returns the index just past its close.
///
/// Rust's block comments nest, so this counts depth rather than stopping at the
/// first `*/`. An unterminated comment blanks the rest of the input: such a file
/// does not compile, so there is no legitimate code left to hide.
fn block_comment_end(bytes: &[u8], start: usize) -> Option<usize> {
    if !starts_with(bytes, start, b"/*") {
        return None;
    }

    let mut i = start + 2;
    let mut depth = 1usize;
    while i < bytes.len() {
        if starts_with(bytes, i, b"/*") {
            depth += 1;
            i += 2;
        } else if starts_with(bytes, i, b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Some(i);
            }
        } else {
            i += 1;
        }
    }
    Some(bytes.len())
}

/// If a line comment starts at `start` (`//`, `///` and `//!` alike), returns the
/// index of the terminating newline — which is left in place, so the line count
/// is unchanged.
fn line_comment_end(bytes: &[u8], start: usize) -> Option<usize> {
    if !starts_with(bytes, start, b"//") {
        return None;
    }

    let mut i = start + 2;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    Some(i)
}

/// If a string literal starts at `start`, returns the index just past its
/// closing quote (or the end of the input, for an unterminated literal).
///
/// Covers all four spellings: `"…"` and its byte form `b"…"`, where a `\`
/// escapes the next character, and the raw forms `r"…"`, `r#"…"#` (any number of
/// hashes) and `br#"…"#`, where nothing escapes and the literal ends only at a
/// quote followed by the *same* number of hashes.
///
/// The `r#` of a raw **identifier** (`r#gtk4`, which the layering guard must
/// still see) is not mistaken for a raw string, because no quote follows its
/// hashes. A prefix character is likewise unambiguous: Rust does not allow an
/// identifier to sit directly against a quote, so a `b`/`r` immediately before
/// one can only be a literal prefix.
fn string_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    if bytes.get(i) == Some(&b'b') {
        i += 1;
    }
    let raw = bytes.get(i) == Some(&b'r');
    if raw {
        i += 1;
    }

    // Only a raw literal may carry hashes between its prefix and its quote.
    let hashes_start = i;
    if raw {
        while bytes.get(i) == Some(&b'#') {
            i += 1;
        }
    }
    let hashes = i - hashes_start;

    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;

    while i < bytes.len() {
        match bytes[i] {
            // In a non-raw literal a backslash escapes the next byte, `\"`
            // included, so the quote it protects does not close the literal.
            b'\\' if !raw => i += 2,
            b'"' => {
                let close = i + 1;
                // A raw literal closes only on as many hashes as it opened with;
                // a plain one needs none, so this is trivially true for it.
                let following_hashes = bytes[close..].iter().take_while(|&&b| b == b'#').count();
                if following_hashes >= hashes {
                    return Some(close + hashes);
                }
                i = close;
            }
            _ => i += 1,
        }
    }
    Some(bytes.len())
}

/// If a character literal starts at `start`, returns the index just past its
/// closing quote.
///
/// Deliberately narrow, because `'` is ambiguous in Rust: besides a character
/// literal it introduces a **lifetime** (`&'a str`) or a loop label
/// (`'outer: loop`), neither of which may be blanked — a lifetime has no closing
/// quote, so treating one as a literal would swallow whatever code follows it.
/// The closing quote is therefore the test: a literal is accepted only when the
/// quote appears exactly where a literal would put it, one character later, or —
/// for an escape such as `'\''`, `'\n'` or `'\u{2014}'` — within the few bytes an
/// escape can span. Byte literals (`b'c'`) are included.
fn char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    /// Covers the escape bodies that occur in practice, the longest being
    /// `\u{10FFFF}` at 9 bytes before the closing quote. The bound is what keeps a
    /// lifetime from being mistaken for a literal whose quote is far away.
    ///
    /// It is not the true maximum: Rust also permits underscores inside a unicode
    /// escape, so `'\u{10_FFFF}'` is legal and longer than this. Such a literal is
    /// then simply not recognized as one — which is safe by construction, because an
    /// unmatched `'` opens nothing in this scanner and an escape body (hex digits,
    /// underscores, braces) cannot spell a forbidden crate name. Widening the bound
    /// would cost more than the case is worth; falling back to "not a literal" is
    /// the direction that cannot hide code.
    const MAX_ESCAPE_BYTES: usize = 9;

    let mut i = start;
    if bytes.get(i) == Some(&b'b') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'\'') {
        return None;
    }
    i += 1;

    if bytes.get(i) == Some(&b'\\') {
        // Skip the backslash and the character it escapes — which may itself be
        // the quote (`'\''`) — then expect the closing quote within the bound.
        i += 2;
        let limit = (i + MAX_ESCAPE_BYTES).min(bytes.len());
        while i < limit {
            if bytes[i] == b'\'' {
                return Some(i + 1);
            }
            i += 1;
        }
        return None;
    }

    // A single character, which may be multi-byte, then the closing quote.
    let char_len = utf8_char_len(*bytes.get(i)?);
    if bytes.get(i + char_len) == Some(&b'\'') {
        Some(i + char_len + 1)
    } else {
        None
    }
}

/// The length in bytes of the UTF-8 character whose leading byte is `lead`.
fn utf8_char_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        // Includes the continuation-byte range (0x80..=0xBF), which cannot be a
        // lead byte in valid UTF-8; treating it as one byte keeps the scan
        // advancing rather than panicking on malformed input.
        _ => 4,
    }
}

/// Whether `bytes` contains `needle` at `at` — a bounds-safe `starts_with` for
/// the two-byte comment markers.
fn starts_with(bytes: &[u8], at: usize, needle: &[u8]) -> bool {
    bytes[at..].starts_with(needle)
}

#[test]
fn comments_are_blanked_but_line_numbers_survive() {
    let source = "use std::fs; // gtk4::Window\n/* gtk4::Button */ let x = 1;\nuse gtk4::Label;\n";
    let code = strip_comments(source);

    let lines: Vec<&str> = code.lines().collect();
    assert_eq!(lines.len(), 3, "line count must be preserved: {lines:?}");
    assert_eq!(lines[0].trim(), "use std::fs;");
    assert_eq!(lines[1].trim(), "let x = 1;");
    // Real code is untouched, so the guards still see the import on line 3.
    assert_eq!(lines[2], "use gtk4::Label;");
}

#[test]
fn a_multi_line_block_comment_keeps_its_newlines() {
    let source = "let a = 1;\n/* gtk4::Window\n   gtk4::Button */\nlet b = 2;\n";
    let code = strip_comments(source);

    let lines: Vec<&str> = code.lines().collect();
    assert_eq!(lines.len(), 4, "line count must be preserved: {lines:?}");
    assert!(
        !code.contains("gtk4"),
        "a commented-out crate name must not survive: {code:?}"
    );
    assert_eq!(lines[3], "let b = 2;");
}

#[test]
fn a_url_string_does_not_blank_the_code_after_it() {
    // Evasion 1 against the pre-hardening lexer: the `//` in a URL was read as
    // starting a line comment, so everything after it on the line — here a real
    // toolkit reference — was blanked and the guard passed. This exact line
    // compiles, and `cargo fmt --check` accepts it.
    let source = "fn probe(s: &str) -> bool { matches!((s == \"https://a\", ::gtk4::Align::Fill), (true, _)) }\n";
    let code = strip_comments(source);

    assert!(
        code.contains("::gtk4::Align"),
        "code following a string containing `//` must survive: {code:?}"
    );
    assert!(
        !code.contains("https"),
        "the string's own contents must still be blanked: {code:?}"
    );
}

#[test]
fn a_glob_string_does_not_open_a_phantom_block_comment() {
    // Evasion 2, the worse one: the `/*` in a glob pattern opened a block comment
    // that blanked every following line until some later `*/`, hiding a plain
    // `gtk4::` reference — a spelling the guard has always claimed to catch.
    // `src/ui/sound.rs` really does contain such a string, harmless only because
    // a later line happens to close the phantom comment.
    let source = "fn pattern() -> &'static str { \"/*.conf\" }\nfn probe() -> gtk4::Align { gtk4::Align::Fill }\n";
    let code = strip_comments(source);

    let lines: Vec<&str> = code.lines().collect();
    assert_eq!(lines.len(), 2, "line count must be preserved: {lines:?}");
    assert!(
        lines[1].contains("gtk4::Align"),
        "a later line must not be blanked by a string containing `/*`: {code:?}"
    );
}

#[test]
fn raw_strings_are_blanked_without_swallowing_the_code_after_them() {
    // Raw strings escape nothing, so they end only at a quote followed by as many
    // hashes as they opened with — the embedded `"# ` and quotes below do not end
    // these literals early.
    let source = concat!(
        "let a = r\"gtk4::A //\";\n",
        "let b = r#\"gtk4::B \"quoted\" /*\"#;\n",
        "let c = br##\"gtk4::C \"# still inside\"##;\n",
        "use gtk4::Label;\n"
    );
    let code = strip_comments(source);

    for hidden in ["gtk4::A", "gtk4::B", "gtk4::C", "quoted", "still inside"] {
        assert!(
            !code.contains(hidden),
            "string contents must be blanked, found {hidden:?} in {code:?}"
        );
    }
    // The import after all three literals is still visible to the guards.
    assert!(
        code.lines().nth(3) == Some("use gtk4::Label;"),
        "code after the raw strings must survive: {code:?}"
    );
}

#[test]
fn an_escaped_quote_does_not_end_a_string_early() {
    // `\"` keeps the literal open, so the `gtk4::` after it is still inside the
    // string; the real reference on the next line is not.
    let source = "let s = \"a \\\" gtk4::Hidden\";\nuse gtk4::Visible;\n";
    let code = strip_comments(source);

    assert!(
        !code.contains("gtk4::Hidden"),
        "an escaped quote must not end the literal: {code:?}"
    );
    assert!(
        code.contains("use gtk4::Visible;"),
        "the following line must survive: {code:?}"
    );
}

#[test]
fn a_raw_identifier_is_not_mistaken_for_a_raw_string() {
    // `r#gtk4` is a raw *identifier* — legal, since `gtk4` is not a keyword — and
    // the layering guard is expected to flag it, so the lexer must leave it alone.
    // What distinguishes it from `r#"…"#` is that no quote follows the hash.
    let code = strip_comments("use r#gtk4::Label;\n");
    assert_eq!(code, "use r#gtk4::Label;\n");
}

#[test]
fn quote_characters_and_lifetimes_are_told_apart() {
    // A `'"'` literal must be consumed (else it opens a phantom string that runs
    // to the next quote — `src/parsers/swaync.rs` has two, five lines apart),
    // while a lifetime has no closing quote and must be left alone, or the code
    // after it would be blanked.
    let source = "if c == '\"' { one() }\nfn f<'a>(w: &'a gtk4::Widget) {}\n";
    let code = strip_comments(source);

    let lines: Vec<&str> = code.lines().collect();
    assert!(
        lines[0].contains("one()"),
        "code after a quote character literal must survive: {code:?}"
    );
    assert!(
        lines[1].contains("&'a gtk4::Widget"),
        "a lifetime must not be treated as a character literal: {code:?}"
    );
    // An escaped quote character literal is the other spelling of the same trap.
    assert!(
        strip_comments("if c == '\\'' { two() }\n").contains("two()"),
        "code after an escaped quote literal must survive"
    );
}

#[test]
fn nested_block_comments_close_at_the_outer_marker() {
    // Rust nests block comments, so the inner `*/` must not end the outer one and
    // expose the crate name that follows it.
    let source = "/* outer /* inner */ gtk4::Hidden */\nuse gtk4::Visible;\n";
    let code = strip_comments(source);

    assert!(
        !code.contains("gtk4::Hidden"),
        "a nested block comment must stay blanked: {code:?}"
    );
    assert!(
        code.contains("use gtk4::Visible;"),
        "the following line must survive: {code:?}"
    );
}

#[test]
fn non_ascii_source_keeps_its_ascii_tokens() {
    // The em dash occupies three bytes; each is replaced by a space, which must
    // not disturb the surrounding ASCII the guards search for.
    let code = strip_comments("let s = \"—\"; use gtk4::Label;\n");
    assert!(
        code.contains("use gtk4::Label;"),
        "ASCII tokens must survive non-ASCII neighbours: {code:?}"
    );
}

#[test]
fn rust_sources_recurses_and_ignores_non_rust_files() {
    // Scan this crate's own `src/parsers` directory: it is known to contain
    // several `.rs` files, and `src/` contains nested module directories.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let files = rust_sources(&manifest_dir.join("src"));

    assert!(
        files
            .iter()
            .all(|f| f.extension().is_some_and(|e| e == "rs")),
        "only .rs files may be returned: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.ends_with("parsers/hyprlang.rs")),
        "the walk must recurse into subdirectories: {files:?}"
    );
}
