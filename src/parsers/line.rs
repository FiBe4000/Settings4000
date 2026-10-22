//! Format-invariant helpers for the line-based parsers (task 9.18).
//!
//! Every surgical parser in this crate (`kv`, `hyprlang`, `lua_literal`,
//! `monitors`, `ini`, `env`) uses the same lossless representation: the input is
//! split after each `\n` into raw lines that keep their terminator attached, each
//! raw line is classified once, and an edit splices bytes inside a single raw line
//! (or inserts a whole new one). Emitting is concatenating the raw lines again.
//!
//! The pieces of that model that do not depend on the file format live here:
//! splitting the input, re-joining it, removing a terminator before a line's
//! content is inspected, choosing the terminator for a newly added line,
//! terminating an unterminated final line before something is appended after it,
//! and finding a character a value must not contain. None of them looks at what a
//! line *means*, which is why one copy can serve every format.
//!
//! What a line means is deliberately **not** shared: each parser keeps its own
//! line classifier, its own addressing model (section paths, repeatable keys,
//! positional record fields, Lua table paths) and its own warning and error types.
//! Those encode the grammar of one format, differ between formats in ways that
//! matter, and are easier to audit when each parser reads top to bottom on its own
//! (architecture §3).

use std::str::SplitInclusive;

/// The characters that would split one logical line into two when written into a
/// value or key: a line feed, and a carriage return (which some readers also treat
/// as a line break). Every writer refuses at least these; formats with more
/// structural characters pass a larger set to [`find_forbidden`].
pub(super) const LINE_BREAKS: [char; 2] = ['\n', '\r'];

/// Access to the exact original bytes of one parsed line, terminator included.
///
/// Each parser has its own line struct (the classification it stores differs per
/// format), but all of them keep the raw bytes, which is all the helpers in this
/// module need. Implementing this one method lets a parser use [`emit`] and the
/// terminator pickers on its own line type.
pub(super) trait RawLine {
    /// The line's bytes exactly as they will be emitted, including the trailing
    /// `\n` or `\r\n` (or none, for a final line without a trailing newline).
    fn raw(&self) -> &str;
}

/// Splits `input` into raw lines, each ending just after its `\n`.
///
/// The terminator stays attached to its line and a final line without a trailing
/// newline is yielded as-is, so concatenating the pieces gives back exactly
/// `input`; an empty input yields no pieces and so round-trips to empty. This is
/// what makes emission byte-for-byte lossless. A `\r` before the `\n` is part of
/// the piece too, so CRLF files survive unchanged.
pub(super) fn split_raw_lines(input: &str) -> SplitInclusive<'_, char> {
    input.split_inclusive('\n')
}

/// Concatenates the raw lines back into file text.
///
/// For an unedited file this is byte-for-byte the parsed input (the round-trip
/// identity every parser's tests check); after an edit it differs only inside the
/// edited spans and any inserted lines.
pub(super) fn emit<L: RawLine>(lines: &[L]) -> String {
    lines.iter().map(RawLine::raw).collect()
}

/// Returns `raw` without its line terminator, so token and value-span
/// computation never runs into it.
///
/// A trailing `\n` is removed and then one `\r` before it, if present. A `\r` at
/// the very end of a line with no `\n` is removed as well; that can only happen on
/// a file's final, unterminated line, and treating the stray `\r` as a terminator
/// keeps it out of the last value. The result is always a prefix of `raw`, so byte
/// offsets into it are offsets into `raw` as well. Callers keep `raw` itself for
/// emission.
pub(super) fn strip_terminator(raw: &str) -> &str {
    let without_lf = raw.strip_suffix('\n').unwrap_or(raw);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

/// Returns `raw` without a trailing `\r\n` or `\n`, and nothing else.
///
/// Unlike [`strip_terminator`], a lone `\r` at the end of an unterminated final
/// line is kept as content. The Lua-literal editor (`lua_literal`) was written
/// with this variant; its lexer skips a `\r` between tokens as whitespace, so in
/// ordinary code the stray byte is harmless. The variant is kept rather than
/// switched to [`strip_terminator`] so the refactor that introduced this module
/// changed no parser's behaviour, not even on that edge case.
pub(super) fn strip_newline(raw: &str) -> &str {
    raw.strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(raw)
}

/// The terminator for a line the app adds to a file: `\r\n` if any existing line
/// ends with `\r\n`, otherwise `\n` (also for an empty file).
///
/// The real dotfiles use LF throughout; the rule mainly keeps an all-CRLF file
/// internally consistent. In a mixed file a single CRLF line is enough to pick
/// CRLF. Used by every parser except `kv`, which follows
/// [`terminator_of_last_terminated_line`].
pub(super) fn terminator_if_any_crlf<L: RawLine>(lines: &[L]) -> &'static str {
    if lines.iter().any(|line| line.raw().ends_with("\r\n")) {
        "\r\n"
    } else {
        "\n"
    }
}

/// The terminator for a line appended at the end of a file: the terminator of
/// the last line that has one, or `\n` when no line is terminated.
///
/// This is the flat `key=value` parser's (`kv`) rule. It agrees with
/// [`terminator_if_any_crlf`] for every file whose lines all use one style and
/// differs only for a mixed file whose last terminated line is LF while an earlier
/// one is CRLF (here: LF). Both rules are kept so neither parser changes
/// behaviour.
pub(super) fn terminator_of_last_terminated_line<L: RawLine>(lines: &[L]) -> &'static str {
    match lines
        .iter()
        .rev()
        .map(RawLine::raw)
        .find(|raw| raw.ends_with('\n'))
    {
        Some(raw) if raw.ends_with("\r\n") => "\r\n",
        _ => "\n",
    }
}

/// Appends `terminator` to `raw` unless it already ends with `\n`.
///
/// Called on the line that a new line will follow, so the new line starts on its
/// own line instead of running onto the previous one. Only a file's final,
/// unterminated line can lack a `\n`, so in practice this adds the trailing
/// newline that file was missing.
pub(super) fn ensure_terminated(raw: &mut String, terminator: &str) {
    if !raw.ends_with('\n') {
        raw.push_str(terminator);
    }
}

/// The first character of `value`, in reading order, that appears in
/// `forbidden`, or `None` when `value` is free of all of them.
///
/// The structural check every writer runs before splicing a value or key into a
/// line: a line break would split the line, and each format adds its own
/// structural characters (a `#` that starts a comment, a `,` that separates
/// fields, …). Each parser passes its own set and turns a hit into its own error
/// type; returning the character lets an error message name it.
pub(super) fn find_forbidden(value: &str, forbidden: &[char]) -> Option<char> {
    value.chars().find(|c| forbidden.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal line type, so the helpers are tested independently of any
    /// parser's own line struct.
    struct TestLine(String);

    impl RawLine for TestLine {
        fn raw(&self) -> &str {
            &self.0
        }
    }

    fn lines_of(input: &str) -> Vec<TestLine> {
        split_raw_lines(input)
            .map(|raw| TestLine(raw.to_string()))
            .collect()
    }

    #[test]
    fn split_and_emit_round_trip_every_terminator_style() {
        for input in [
            "",
            "\n",
            "a\nb\n",
            "a\r\nb\r\n",
            "a\nb",
            "a\r\nb",
            "a\rb\n",
            "a\r",
            "a\n\r\nb\r\n\n",
        ] {
            assert_eq!(emit(&lines_of(input)), input, "round trip of {input:?}");
        }
    }

    #[test]
    fn split_keeps_terminators_attached_and_an_unterminated_tail_as_is() {
        let pieces: Vec<&str> = split_raw_lines("a\r\nb\nc").collect();
        assert_eq!(pieces, ["a\r\n", "b\n", "c"]);
        assert_eq!(split_raw_lines("").count(), 0, "empty input has no lines");
        // A lone CR is not a line break for splitting.
        let pieces: Vec<&str> = split_raw_lines("a\rb\n").collect();
        assert_eq!(pieces, ["a\rb\n"]);
    }

    #[test]
    fn strip_terminator_removes_lf_crlf_and_a_trailing_lone_cr() {
        assert_eq!(strip_terminator("key=v\n"), "key=v");
        assert_eq!(strip_terminator("key=v\r\n"), "key=v");
        assert_eq!(strip_terminator("key=v"), "key=v", "no terminator");
        assert_eq!(strip_terminator("key=v\r"), "key=v", "lone CR at the end");
        assert_eq!(strip_terminator(""), "");
        assert_eq!(strip_terminator("\n"), "");
        // Only one CR belongs to the terminator; an earlier one is content.
        assert_eq!(strip_terminator("key=v\r\r\n"), "key=v\r");
        // A CR in the middle of a line is never touched.
        assert_eq!(strip_terminator("a\rb\n"), "a\rb");
    }

    #[test]
    fn strip_newline_keeps_a_trailing_lone_cr() {
        assert_eq!(strip_newline("x = 1,\n"), "x = 1,");
        assert_eq!(strip_newline("x = 1,\r\n"), "x = 1,");
        assert_eq!(strip_newline("x = 1,"), "x = 1,", "no terminator");
        assert_eq!(strip_newline("x = 1,\r"), "x = 1,\r", "lone CR is content");
        assert_eq!(strip_newline(""), "");
        assert_eq!(strip_newline("x\r\r\n"), "x\r");
    }

    #[test]
    fn strip_results_are_prefixes_of_the_raw_line() {
        for raw in ["a=b\n", "a=b\r\n", "a=b", "a=b\r", ""] {
            assert!(raw.starts_with(strip_terminator(raw)));
            assert!(raw.starts_with(strip_newline(raw)));
        }
    }

    #[test]
    fn any_crlf_rule_picks_crlf_when_any_line_uses_it() {
        assert_eq!(terminator_if_any_crlf(&lines_of("a\nb\n")), "\n");
        assert_eq!(terminator_if_any_crlf(&lines_of("a\r\nb\r\n")), "\r\n");
        assert_eq!(terminator_if_any_crlf(&lines_of("")), "\n", "empty file");
        assert_eq!(
            terminator_if_any_crlf(&lines_of("a")),
            "\n",
            "no terminator"
        );
        // A lone CR is not a CRLF terminator.
        assert_eq!(terminator_if_any_crlf(&lines_of("a\r")), "\n");
        // Mixed: one CRLF line anywhere is enough.
        assert_eq!(terminator_if_any_crlf(&lines_of("a\r\nb\nc\n")), "\r\n");
        assert_eq!(terminator_if_any_crlf(&lines_of("a\nb\nc\r\n")), "\r\n");
    }

    #[test]
    fn last_terminated_rule_follows_the_last_line_that_has_a_terminator() {
        assert_eq!(
            terminator_of_last_terminated_line(&lines_of("a\nb\n")),
            "\n"
        );
        assert_eq!(
            terminator_of_last_terminated_line(&lines_of("a\r\nb\r\n")),
            "\r\n"
        );
        assert_eq!(terminator_of_last_terminated_line(&lines_of("")), "\n");
        assert_eq!(terminator_of_last_terminated_line(&lines_of("a")), "\n");
        assert_eq!(terminator_of_last_terminated_line(&lines_of("a\r")), "\n");
        // An unterminated tail is skipped in favour of the line before it.
        assert_eq!(
            terminator_of_last_terminated_line(&lines_of("a\r\n# tail")),
            "\r\n"
        );
        // Mixed: the last terminated line decides, unlike the any-CRLF rule.
        assert_eq!(
            terminator_of_last_terminated_line(&lines_of("a\r\nb\nc\n")),
            "\n"
        );
        assert_eq!(
            terminator_of_last_terminated_line(&lines_of("a\nb\nc\r\n")),
            "\r\n"
        );
    }

    #[test]
    fn ensure_terminated_adds_a_terminator_only_when_missing() {
        let mut raw = "a=1".to_string();
        ensure_terminated(&mut raw, "\r\n");
        assert_eq!(raw, "a=1\r\n");

        let mut raw = "a=1\n".to_string();
        ensure_terminated(&mut raw, "\r\n");
        assert_eq!(raw, "a=1\n", "an LF line keeps its own terminator");

        let mut raw = "a=1\r\n".to_string();
        ensure_terminated(&mut raw, "\n");
        assert_eq!(raw, "a=1\r\n");

        // A lone trailing CR is not a terminator, so one is still added.
        let mut raw = "a=1\r".to_string();
        ensure_terminated(&mut raw, "\n");
        assert_eq!(raw, "a=1\r\n");
    }

    #[test]
    fn find_forbidden_reports_the_first_hit_in_value_order() {
        assert_eq!(find_forbidden("plain value", &LINE_BREAKS), None);
        assert_eq!(find_forbidden("", &LINE_BREAKS), None);
        assert_eq!(find_forbidden("a\nb", &LINE_BREAKS), Some('\n'));
        assert_eq!(find_forbidden("a\rb", &LINE_BREAKS), Some('\r'));
        // The order of the value decides, not the order of the set.
        assert_eq!(find_forbidden("a#b,c", &[',', '#']), Some('#'));
        assert_eq!(find_forbidden("a,b#c", &['#', ',']), Some(','));
        assert_eq!(find_forbidden("anything", &[]), None, "empty set");
    }
}
