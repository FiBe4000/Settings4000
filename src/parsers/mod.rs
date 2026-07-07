//! Config-file parsers and surgical writers (architecture §3).
//!
//! One submodule per file format the app edits (palette `key=hex`, hyprlang,
//! `monitors.conf`, swaync JSON, GTK INI, `uwsm/env`). Each parser produces a
//! lossless line/token representation and rewrites only the value span of a
//! targeted key, leaving comments, ordering, and commented-out lines
//! byte-identical. Every parser carries round-trip tests (`parse → edit
//! nothing → emit == input`).
//!
//! Hard layering rule: like [`crate::core`], nothing here may import `gtk` or
//! `relm4` — parsers are pure, display-free, and independently testable
//! (enforced by `tests/module_boundaries.rs`).
