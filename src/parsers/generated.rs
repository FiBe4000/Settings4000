//! Read-only readers for the palette pipeline's *generated* files
//! (task 3.7; architecture §3; R3.2, R8.5, R6.1).
//!
//! # What this file is, and why it is read-only
//!
//! Running `scripts/generate-colors <scheme>` regenerates a fixed set of ten
//! files from the single palette source `colors/<scheme>`: six color partials,
//! three font partials, and a `state/active-scheme` marker (analysis §6.4, §6.5,
//! §8.3 — listed in [`GENERATED_COLOR_PARTIALS`], [`GENERATED_FONT_PARTIALS`] and
//! [`ACTIVE_SCHEME_MARKER`]). Each partial starts with the header
//! `# Generated from colors/<scheme> — do not edit manually` (or `theme/fonts`
//! for the font partials) behind its format's comment marker: bare for hyprlang,
//! kitty and zsh, `// ` for SCSS, `/* … */` for rasi and `-- ` for the Lua
//! partial `colors.lua` (see [`HEADER_COMMENT_MARKERS`]). Every one of those files
//! is a **read-only input** to this app: the app never writes them, because a
//! palette change goes through re-running `generate-colors` (R3.2), not a hand
//! edit of the outputs. Accordingly this module has **no write/edit path at
//! all** — only parsing/reading — in deliberate contrast to the sibling parsers,
//! which all expose a surgical writer.
//!
//! The former `config/swaync/colors.css` partial is no longer generated (the
//! swaync notification daemon was replaced, analysis §8.3) and is not part of
//! the set.
//!
//! # What it provides
//!
//! - **Active-scheme detection.** The name of the currently applied scheme is read
//!   from the `# Generated from colors/<scheme>` header of the deployed
//!   `~/.config/hypr/colors.conf` (R3.2). Any failure to recognize that header
//!   — a missing header, an odd/malformed header, an empty file, or an unreadable
//!   file — degrades to [`ActiveScheme::Unknown`] rather than an error or panic,
//!   so a machine without this dotfiles setup never breaks detection.
//! - **Per-scheme swatch parse.** Given the contents of a `colors/<scheme>` source
//!   file (located via the repo root discovered from a deployed config symlink,
//!   R8.5 — that discovery is task 4.3's job, so this module simply accepts the
//!   contents or a path), [`parse_scheme_swatch`] reuses the task-3.1 palette
//!   parser ([`super::palette::PaletteFile`]) to read the schema colors (18
//!   today, whatever the given [`PaletteSchema`] lists) for swatch previews in
//!   the theme drop-down (task 6.3). It stays read-only: it reads values
//!   through the palette parser and never edits.
//!
//! # The `state/active-scheme` marker
//!
//! `generate-colors` also writes a one-line `state/active-scheme` marker file at
//! the repo root (deliberately outside `colors/`, and not symlinked — analysis
//! §6.4). It is an *optional future shortcut* for active-scheme detection. v1 does
//! not use it: it reads the header of the deployed `colors.conf`, which is present
//! at a stable XDG path even when the repo root cannot be resolved. This module
//! could gain a marker-based reader later without changing the header-based
//! behavior documented here.

use std::path::Path;

use super::palette::{PaletteFile, PaletteSchema, SchemaValidation};

/// The color partials `generate-colors` writes, as repo-relative paths
/// (analysis §8.3). Each is headed `… Generated from colors/<scheme> …`, so any
/// of them names the active scheme; the app reads only `config/hypr/colors.conf`
/// for that (R3.2), because it is the one with a stable deployed path the
/// palette-source discovery already relies on.
///
/// `config/hypr/colors.lua` is the Lua-provider copy of the palette
/// (`hyprland.lua` loads it with `dofile`, analysis §7.6). It sits beside
/// `hyprland.lua`, which the cursor setting *does* write, so it is listed here
/// explicitly: no app write may ever target it.
pub const GENERATED_COLOR_PARTIALS: [&str; 6] = [
    "config/hypr/colors.conf",
    "config/hypr/colors.lua",
    "config/eww/_colors.scss",
    "config/rofi/colors.rasi",
    "config/kitty/colors.conf",
    "zsh/colors.zsh",
];

/// The font partials `generate-colors` templates from `theme/fonts`, as
/// repo-relative paths (analysis §6.5). Their header names `theme/fonts`
/// instead of a scheme. Fonts are out of v1 scope; the set is listed so the
/// "never a write target" rule covers every generator output.
pub const GENERATED_FONT_PARTIALS: [&str; 3] = [
    "config/kitty/fonts.conf",
    "config/eww/_fonts.scss",
    "config/rofi/fonts.rasi",
];

/// The repo-relative path of the one-line active-scheme marker
/// `generate-colors` writes last (analysis §6.4). It has no header.
pub const ACTIVE_SCHEME_MARKER: &str = "state/active-scheme";

/// The comment markers that may wrap a generator header in front of its `#`:
/// `-- # Generated …` (Lua), `// # Generated …` (SCSS), `/* # Generated … */`
/// (rasi). A bare `# Generated …` (hyprlang, kitty, zsh) needs none.
///
/// The generator builds every header from one `# Generated from …` string and
/// prefixes it with the target format's comment marker, which is why the `#`
/// is still required after the marker.
pub const HEADER_COMMENT_MARKERS: [&str; 3] = ["--", "//", "/*"];

/// Every file `generate-colors` writes, as repo-relative paths: the
/// [color](GENERATED_COLOR_PARTIALS) and [font](GENERATED_FONT_PARTIALS)
/// partials and the [marker](ACTIVE_SCHEME_MARKER) — ten in total.
///
/// They are read-only inputs (module docs). Running the generator is the only
/// way they change, and its run is the last write step of an Apply because a
/// rollback restores only files the app wrote itself (architecture §6).
pub fn generated_files() -> impl Iterator<Item = &'static str> {
    GENERATED_COLOR_PARTIALS
        .into_iter()
        .chain(GENERATED_FONT_PARTIALS)
        .chain(std::iter::once(ACTIVE_SCHEME_MARKER))
}

/// The palette scheme currently applied, as detected from a generated file's
/// header.
///
/// Detection is best-effort and never fails: a header that cannot be recognized
/// yields [`ActiveScheme::Unknown`] (see the module docs for the exact
/// tolerances). The theme page (task 6.3) uses this to preselect the active
/// scheme in its drop-down, and falls back to a neutral "unknown" display when it
/// cannot be determined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActiveScheme {
    /// A scheme name was extracted from a `# Generated from colors/<scheme>`
    /// header (e.g. `everforest`). The string is the `<scheme>` token exactly as
    /// it appeared after `colors/`, with its original case preserved (scheme names
    /// are file names, so case is significant).
    Named(String),
    /// No scheme could be determined: the header was absent, malformed, or the
    /// source file was empty or unreadable. This is a normal "not applicable"
    /// state, never an error.
    Unknown,
}

impl ActiveScheme {
    /// The detected scheme name, or `None` when [`Unknown`](Self::Unknown).
    ///
    /// A small convenience so callers can treat detection as an `Option` without
    /// matching the enum.
    pub fn name(&self) -> Option<&str> {
        match self {
            ActiveScheme::Named(name) => Some(name),
            ActiveScheme::Unknown => None,
        }
    }
}

/// Detects the active palette scheme from the contents of a deployed generated
/// file (in v1, `~/.config/hypr/colors.conf`).
///
/// It scans for the first line that matches the generator's header shape,
/// `# Generated from colors/<scheme> …` (analysis §6.4, §2), optionally behind
/// one of the [`HEADER_COMMENT_MARKERS`] (so the header of any
/// [color partial](GENERATED_COLOR_PARTIALS), `colors.lua`'s
/// `-- # Generated …` included, is recognised), and returns the `<scheme>`
/// token. The header is conventionally the file's first line, but this
/// scans every line and returns the first match; that tolerates an incidental
/// leading blank or comment line without risk, because the `Generated from
/// colors/` shape is specific to the generator's header and does not occur in the
/// generated body (which is `$key = rgb(…)` assignments, SCSS, etc.).
///
/// Recognition is deliberately tolerant but does not over-match: leading
/// whitespace, a missing space after `#` or after the comment marker,
/// arbitrary internal whitespace, and
/// ASCII-case differences in the words `Generated`/`from` and the `colors/` prefix
/// are all accepted, while the two words and the literal `colors/` prefix followed
/// by a non-empty scheme token are all *required*. Anything else yields
/// [`ActiveScheme::Unknown`].
pub fn detect_active_scheme(generated_file_contents: &str) -> ActiveScheme {
    for line in generated_file_contents.lines() {
        if let Some(scheme) = scheme_from_header_line(line) {
            return ActiveScheme::Named(scheme);
        }
    }
    ActiveScheme::Unknown
}

/// Reads a deployed generated file from disk and detects the active scheme from
/// its header, degrading to [`ActiveScheme::Unknown`] when the file cannot be
/// read.
///
/// This is the entry point the store/detection layer (tasks 4.2/4.3) calls with
/// the live XDG path of `colors.conf`. An unreadable file (absent, permission
/// revoked, or invalid UTF-8) is a normal "absent" condition on a machine without
/// this dotfiles setup, so it is logged at `debug` and treated as unknown — never
/// an error that could abort startup (R4, R8.5). Only the path is logged, never
/// the file contents (R7.3).
pub fn read_active_scheme(generated_file_path: &Path) -> ActiveScheme {
    match std::fs::read_to_string(generated_file_path) {
        Ok(contents) => detect_active_scheme(&contents),
        Err(error) => {
            tracing::debug!(
                path = %generated_file_path.display(),
                %error,
                "generated colors file unreadable; active scheme unknown"
            );
            ActiveScheme::Unknown
        }
    }
}

/// The colors of one palette scheme, read for swatch display (task 6.3).
///
/// Produced by [`parse_scheme_swatch`] from a `colors/<scheme>` source file. It
/// carries the schema colors that are actually present, in the schema's
/// order, plus the palette's [`SchemaValidation`] so a consumer can tell a
/// complete, well-formed scheme from a broken one (e.g. one missing a key). It is
/// purely a read view — there is no way to edit through it.
#[derive(Debug, PartialEq, Eq)]
pub struct SchemeSwatch {
    /// The schema colors present in the file, in the [`PaletteSchema`]'s order.
    /// A schema key with no entry in the file is omitted (a swatch can only show a
    /// color it has), so on a valid palette this holds every schema key and on a
    /// malformed one it holds fewer.
    colors: Vec<SwatchColor>,
    /// The palette's schema check (missing/unknown/duplicate keys). Lets the theme
    /// page flag or skip a malformed scheme; a swatch is still returned regardless
    /// so detection never fails.
    validation: SchemaValidation,
}

impl SchemeSwatch {
    /// The present schema colors, in canonical schema order.
    pub fn colors(&self) -> &[SwatchColor] {
        &self.colors
    }

    /// The bare-hex value of `key`, if that schema key is present in the file.
    pub fn color(&self, key: &str) -> Option<&str> {
        self.colors
            .iter()
            .find(|color| color.key == key)
            .map(|color| color.value.as_str())
    }

    /// The palette's schema-validity report, for deciding whether the scheme is
    /// complete and well-formed; it also names the key list it was checked
    /// against.
    pub fn validation(&self) -> &SchemaValidation {
        &self.validation
    }
}

/// One schema color of a scheme: its canonical key and the value found in the
/// file.
///
/// The value is the raw token read by the palette parser (normally a bare-hex
/// color such as `83c092`); it is not re-validated here, since a scheme that
/// carries a non-hex value is already reported through
/// [`SchemeSwatch::validation`] and the palette parser's own warnings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwatchColor {
    /// The schema key (e.g. `bg0`, `accent0`).
    key: String,
    /// The value as read from the file (a bare-hex color on a well-formed scheme).
    value: String,
}

impl SwatchColor {
    /// The schema key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The value as read from the file.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// Parses a `colors/<scheme>` source file's contents into a [`SchemeSwatch`],
/// reusing the task-3.1 palette parser and checking it against `schema`.
///
/// This never fails and never panics: [`PaletteFile::parse`] preserves every line
/// and only surfaces problems as warnings, so a malformed palette simply yields a
/// swatch with whichever schema colors it could read and a [`SchemaValidation`] that
/// reports what is wrong. The result is display-only and read-only — nothing here
/// writes the file.
pub fn parse_scheme_swatch(scheme_contents: &str, schema: &PaletteSchema) -> SchemeSwatch {
    // This parse is used both for the active scheme and to *probe* arbitrary files
    // while enumerating schemes (task 6.3), so the warnings are never surfaced at
    // `warn` (a non-palette file such as a README would otherwise log one per line).
    // The palette parser returns warnings rather than logging them (like the sibling
    // parsers); a swatch only needs the readable values plus the schema report. Since
    // no caller logs palette parse warnings anymore, a single `debug` line records
    // that a present palette parsed with warnings, so a malformed-but-present scheme
    // still leaves a diagnostic trail (R7.3) while a non-palette probe stays quiet at
    // the default log level.
    let (palette, warnings) = PaletteFile::parse(scheme_contents);
    if !warnings.is_empty() {
        // Only the count is logged, never the file contents or the per-line details,
        // to respect R7.3.
        tracing::debug!(
            warnings = warnings.len(),
            "palette scheme parsed with warnings while building its swatch"
        );
    }

    let colors = schema
        .keys()
        .iter()
        .filter_map(|key| {
            palette.value(key).map(|value| SwatchColor {
                key: key.clone(),
                value: value.to_string(),
            })
        })
        .collect();

    SchemeSwatch {
        colors,
        validation: palette.validate(schema),
    }
}

/// Reads a `colors/<scheme>` source file from disk and parses it into a
/// [`SchemeSwatch`] checked against `schema`, returning `None` when the file
/// cannot be read.
///
/// The theme page (task 6.3) calls this per scheme once the repo root has been
/// discovered (task 4.3). A `None` means the file was absent, permission-denied,
/// or not valid UTF-8 — treated as "no swatch for this scheme" rather than an
/// error, consistent with how missing sources degrade elsewhere (R4, R8.5). Only
/// the path is logged, never the file contents (R7.3).
pub fn read_scheme_swatch(scheme_path: &Path, schema: &PaletteSchema) -> Option<SchemeSwatch> {
    match std::fs::read_to_string(scheme_path) {
        Ok(contents) => Some(parse_scheme_swatch(&contents, schema)),
        Err(error) => {
            tracing::debug!(
                path = %scheme_path.display(),
                %error,
                "palette scheme file unreadable; no swatch"
            );
            None
        }
    }
}

/// Extracts the `<scheme>` token from a single line if it is a generator header of
/// the shape `# Generated from colors/<scheme> …`, optionally behind one of the
/// [`HEADER_COMMENT_MARKERS`], otherwise returns `None`.
///
/// Using [`str::split_whitespace`] collapses every run of whitespace, so a header
/// with unusual spacing (`#   Generated   from   colors/nord …`) is handled for
/// free. The prose words are matched case-insensitively and the `colors/` prefix
/// is matched case-insensitively, while the scheme token after the prefix is
/// returned verbatim (case preserved) because it is a file name. An empty scheme
/// token (`colors/` with nothing after it) is rejected.
fn scheme_from_header_line(line: &str) -> Option<String> {
    // A header is a comment: it must start with `#` (after any indentation and
    // an optional format comment marker such as `-- ` or `// `). The space after
    // `#` is optional because `split_whitespace` on the remainder handles both
    // `# Generated` and `#Generated`. A rasi header's closing `*/` is a separate
    // trailing word and is ignored like the "do not edit manually" suffix.
    let line = line.trim_start();
    let line = HEADER_COMMENT_MARKERS
        .iter()
        .find_map(|marker| line.strip_prefix(marker))
        .map_or(line, str::trim_start);
    let comment_body = line.strip_prefix('#')?;

    let mut words = comment_body.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("generated") {
        return None;
    }
    if !words.next()?.eq_ignore_ascii_case("from") {
        return None;
    }

    // The third word is the `colors/<scheme>` path token; the em-dash and the
    // "do not edit manually" suffix are separate words and are ignored.
    let path_token = words.next()?;
    const PREFIX: &str = "colors/";
    // `str::get` returns `None` (never panics) if the token is shorter than the
    // prefix or the split would land inside a multi-byte character.
    let prefix = path_token.get(..PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(PREFIX) {
        return None;
    }

    let scheme = &path_token[PREFIX.len()..];
    if scheme.is_empty() {
        None
    } else {
        Some(scheme.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// A well-formed deployed `~/.config/hypr/colors.conf`: the generator header
    /// (with the em-dash and "do not edit manually" suffix exactly as documented
    /// in analysis §6.4/§2) followed by a few hyprlang color assignments — the
    /// generated body the header sits atop.
    const COLORS_CONF_FIXTURE: &str = "\
# Generated from colors/everforest — do not edit manually
$bg0 = rgb(272e33)
$fg0 = rgb(d3c6aa)
$accent0 = rgb(83c092)
";

    /// A complete, valid `colors/<scheme>` source with all 18 schema keys and
    /// realistic Everforest bare-hex values (analysis §2, §6.4, §7.6).
    const SCHEME_FIXTURE: &str = "\
# Everforest Dark palette
bg0=272e33
bg1=2e383c
bg2=374145
bg3=414b50
fg0=d3c6aa
fg1=9da9a0
fg2=859289
accent0=83c092
accent1=a7c080
accent2=7fbbb3
accent3=d699b6
red=e67e80
orange=e69875
yellow=dbbc7f
green=a7c080
blue=7fbbb3
purple=d699b6
shadow=0a0c0d
";

    #[test]
    fn well_formed_header_extracts_the_scheme_name() {
        // Accept criterion: the scheme name is extracted from a real header (R3.2).
        assert_eq!(
            detect_active_scheme(COLORS_CONF_FIXTURE),
            ActiveScheme::Named("everforest".to_string())
        );
        assert_eq!(
            detect_active_scheme(COLORS_CONF_FIXTURE).name(),
            Some("everforest")
        );
    }

    #[test]
    fn header_recognition_tolerates_whitespace_and_ascii_casing() {
        // Whitespace runs are collapsed and the prose/prefix are case-insensitive,
        // but the scheme token keeps its original case. This must not over-match:
        // the required words and the `colors/` prefix are still all present.
        for input in [
            "#   Generated   from   colors/nord   — do not edit manually\n",
            "#Generated from colors/nord\n",
            "  # generated FROM Colors/nord\n",
        ] {
            assert_eq!(
                detect_active_scheme(input),
                ActiveScheme::Named("nord".to_string()),
                "expected `nord` from {input:?}"
            );
        }

        // A scheme token's own case is preserved verbatim.
        assert_eq!(
            detect_active_scheme("# Generated from colors/MyScheme\n"),
            ActiveScheme::Named("MyScheme".to_string())
        );

        // A hyphenated scheme name (e.g. `gruvbox-dark`) is extracted whole — the
        // hyphen is part of the file name, not a word boundary.
        assert_eq!(
            detect_active_scheme("# Generated from colors/gruvbox-dark — do not edit manually\n"),
            ActiveScheme::Named("gruvbox-dark".to_string())
        );
    }

    #[test]
    fn header_below_a_leading_blank_or_comment_is_still_detected() {
        // The scan tolerates an incidental leading blank/comment line; the header
        // shape is specific enough that this cannot over-match the generated body.
        let input = "\n# some preamble\n# Generated from colors/everforest — do not edit manually\n$bg0 = rgb(272e33)\n";
        assert_eq!(
            detect_active_scheme(input),
            ActiveScheme::Named("everforest".to_string())
        );
    }

    #[test]
    fn missing_malformed_and_empty_headers_degrade_to_unknown_without_error() {
        // Accept criterion: a missing/odd header (and an empty file) degrades to
        // "unknown" without an Err or panic.
        let cases = [
            // Empty file.
            "",
            // No header at all, just generated body.
            "$bg0 = rgb(272e33)\n$fg0 = rgb(d3c6aa)\n",
            // A comment, but not the generator header.
            "# just a normal comment\n",
            // Prose before the header words: the first word is `This`, not
            // `Generated`, so a comment that merely mentions the header text in a
            // sentence must not match.
            "# This file was Generated from colors/x\n",
            // The `from` word is missing, so it is not a header.
            "# Generated colors/everforest\n",
            // The path prefix is not `colors/`.
            "# Generated from schemes/everforest\n",
            "# Generated from colours/everforest\n",
            // `colors/` with no scheme token after it.
            "# Generated from colors/\n",
            "# Generated from colors/ — do not edit manually\n",
            // Header text present but not as a comment.
            "Generated from colors/everforest\n",
        ];
        for input in cases {
            assert_eq!(
                detect_active_scheme(input),
                ActiveScheme::Unknown,
                "expected Unknown from {input:?}"
            );
            assert_eq!(detect_active_scheme(input).name(), None);
        }
    }

    #[test]
    fn read_active_scheme_reads_a_file_and_treats_an_unreadable_path_as_unknown() {
        // The file-backed entry point: a real file with a good header resolves to
        // the scheme...
        let mut file = NamedTempFile::new().expect("create temp colors.conf");
        file.write_all(COLORS_CONF_FIXTURE.as_bytes())
            .expect("write fixture");
        assert_eq!(
            read_active_scheme(file.path()),
            ActiveScheme::Named("everforest".to_string())
        );

        // ...and an unreadable (here, nonexistent) path degrades to Unknown rather
        // than returning an error — covering the "unreadable input" acceptance.
        let missing = Path::new("/nonexistent/settings4000/hypr/colors.conf");
        assert_eq!(read_active_scheme(missing), ActiveScheme::Unknown);
    }

    #[test]
    fn non_utf8_file_contents_degrade_to_unknown_without_error() {
        // A file that exists but is not valid UTF-8 is a distinct "unreadable"
        // case from a missing file: `read_to_string` fails to decode it. It must
        // still degrade to Unknown / None, never an Err or panic (task 3.7's
        // "unreadable input degrades to unknown" criterion).
        let mut file = NamedTempFile::new().expect("create temp file");
        // `0xFF` is never a valid UTF-8 byte, so decoding the file fails.
        file.write_all(&[0xFF, 0xFE, 0x00, 0xFF])
            .expect("write raw bytes");

        assert_eq!(read_active_scheme(file.path()), ActiveScheme::Unknown);
        assert!(read_scheme_swatch(file.path(), &PaletteSchema::built_in()).is_none());
    }

    #[test]
    fn swatch_parse_reads_every_schema_color_via_the_palette_parser() {
        // Accept criterion / R8.5: reuse the palette parser to read the schema
        // colors (18 on the built-in list) for swatch display.
        let schema = PaletteSchema::built_in();
        let swatch = parse_scheme_swatch(SCHEME_FIXTURE, &schema);

        assert_eq!(
            swatch.colors().len(),
            schema.keys().len(),
            "a complete scheme yields every schema color"
        );
        // Values come straight from the palette parser.
        assert_eq!(swatch.color("bg0"), Some("272e33"));
        assert_eq!(swatch.color("accent0"), Some("83c092"));
        assert_eq!(swatch.color("purple"), Some("d699b6"));
        assert_eq!(swatch.color("shadow"), Some("0a0c0d"));
        assert_eq!(swatch.color("not-a-key"), None);

        // The colors are in canonical schema order and expose their key/value.
        let first = &swatch.colors()[0];
        assert_eq!(first.key(), "bg0");
        assert_eq!(first.value(), "272e33");

        // A complete, well-formed scheme is schema-valid.
        assert!(swatch.validation().is_valid());
    }

    #[test]
    fn swatch_parse_degrades_gracefully_on_a_malformed_palette() {
        // A palette missing a key and carrying a junk line must not panic: the
        // swatch simply holds the colors it could read and the validation reports
        // the missing key.
        let malformed = "\
bg0=272e33
this line is not an assignment
accent0=83c092
";
        let swatch = parse_scheme_swatch(malformed, &PaletteSchema::built_in());

        // Only the two real entries are present; the junk line is not a color.
        assert_eq!(swatch.colors().len(), 2);
        assert_eq!(swatch.color("bg0"), Some("272e33"));
        assert_eq!(swatch.color("accent0"), Some("83c092"));
        // A dropped key is simply absent from the swatch...
        assert_eq!(swatch.color("purple"), None);
        // ...and the schema report flags the incompleteness.
        assert!(!swatch.validation().is_valid());
    }

    #[test]
    fn read_scheme_swatch_reads_a_file_and_returns_none_for_a_missing_path() {
        let mut file = NamedTempFile::new().expect("create temp scheme file");
        file.write_all(SCHEME_FIXTURE.as_bytes())
            .expect("write fixture");

        let schema = PaletteSchema::built_in();
        let swatch = read_scheme_swatch(file.path(), &schema).expect("readable scheme file");
        assert_eq!(swatch.colors().len(), schema.keys().len());
        assert_eq!(swatch.color("accent0"), Some("83c092"));

        // A missing file yields None (no swatch), never an error.
        let missing = Path::new("/nonexistent/settings4000/colors/everforest");
        assert!(read_scheme_swatch(missing, &schema).is_none());
    }

    #[test]
    fn every_color_partial_header_shape_names_the_scheme() {
        // The first line of each color partial exactly as the generator's
        // heredocs emit it (analysis §7.6, §8.3), `colors.lua`'s `-- ` marker
        // included.
        for header in [
            "# Generated from colors/everforest — do not edit manually",
            "-- # Generated from colors/everforest — do not edit manually",
            "// # Generated from colors/everforest — do not edit manually",
            "/* # Generated from colors/everforest — do not edit manually */",
        ] {
            let contents = format!("{header}\n\nbody\n");
            assert_eq!(
                detect_active_scheme(&contents),
                ActiveScheme::Named("everforest".to_string()),
                "expected `everforest` from {header:?}"
            );
        }
        // Tight spacing after the marker and a rasi header with no suffix.
        assert_eq!(
            detect_active_scheme("--# Generated from colors/nord\n").name(),
            Some("nord")
        );
        assert_eq!(
            detect_active_scheme("/* # Generated from colors/nord */\n").name(),
            Some("nord")
        );
    }

    #[test]
    fn a_comment_marker_without_the_generator_hash_is_not_a_header() {
        // The generator always keeps the `#` after the format's marker, so a Lua
        // or SCSS comment that merely reads like the header is not one, and an
        // unknown marker is not stripped.
        for input in [
            "-- Generated from colors/everforest\n",
            "// Generated from colors/everforest\n",
            "--- # Generated from colors/everforest\n",
            "; # Generated from colors/everforest\n",
            "return { -- # Generated from colors/everforest\n",
        ] {
            assert_eq!(
                detect_active_scheme(input),
                ActiveScheme::Unknown,
                "expected Unknown from {input:?}"
            );
        }
    }

    #[test]
    fn the_generated_file_set_is_the_ten_outputs_of_the_current_generator() {
        // Analysis §8.3: six color partials (with the Lua one, without the retired
        // swaync stylesheet), three font partials and the marker.
        let files: Vec<&str> = generated_files().collect();
        assert_eq!(files.len(), 10);
        assert!(files.contains(&"config/hypr/colors.lua"));
        assert!(files.contains(&ACTIVE_SCHEME_MARKER));
        assert!(
            !files.iter().any(|file| file.contains("swaync")),
            "swaync/colors.css is no longer generated"
        );
    }
}
