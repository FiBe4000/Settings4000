//! Parse-modify-reserialize adapter for the swaync notification-daemon config
//! (`swaync/config.json`, task 3.4; architecture §3; R5.3 item 1, R6.1).
//!
//! # What this file is
//!
//! swaync (SwayNotificationCenter) stores its settings as a single JSON object
//! at `~/.config/swaync/config.json`. Unlike the other files this app edits, it
//! is real JSON — no comments, no shell fragments. The keys the app's
//! Notifications page (task 6.7) edits are all top-level scalars, for example:
//!
//! ```json
//! {
//!   "positionX": "right",
//!   "positionY": "top",
//!   "timeout": 10,
//!   "timeout-low": 5,
//!   "timeout-critical": 0
//! }
//! ```
//!
//! # Why an adapter, not a line/token parser
//!
//! Every other parser in this module keeps a lossless line/token model and
//! rewrites only a value's byte span, because those formats carry comments,
//! blank-line grouping, and deliberate ordering a maintainer relies on
//! (architecture §3). JSON here carries none of that, so a full
//! parse-modify-reserialize round-trip is acceptable and far simpler
//! (architecture §3 table). The relevant preservation guarantee is therefore not
//! byte-identity but **key-order stability plus canonical formatting**:
//!
//! - The crate enables `serde_json`'s `preserve_order` feature, so a parsed
//!   object keeps its keys in the order they appeared (backed by an index map)
//!   and re-serializing them keeps that order — a no-op edit does not reshuffle
//!   the file.
//! - Re-serialization uses `serde_json`'s pretty printer (two-space indent) plus
//!   a trailing newline, which is exactly the on-disk shape swaync uses. A file
//!   already in that canonical form therefore round-trips byte-for-byte; a file
//!   in some other formatting is normalized to the canonical form. That covers
//!   not just indentation and the trailing newline but any other serializer
//!   normalization — e.g. string-escape style (a raw non-ASCII character vs a
//!   `\uXXXX` escape) and number spelling (`10.0` re-emitted as `10`). These are
//!   the expected formatting normalizations for a comment-free JSON file (see
//!   [`SwayncConfigFile::emit`]).
//!
//! # Scope in v1
//!
//! The get/set API is intentionally format-level only: it reads and writes typed
//! JSON scalars by key and performs no domain validation (e.g. it does not check
//! that `positionX` is one of swaync's accepted values, or that a timeout is in
//! range). Those checks belong to the typed settings model and its validators
//! (task 4.1) and the Notifications page (task 6.7); keeping them out here mirrors
//! how the palette parser separates "is this the file's value format" from
//! higher-level policy.

use std::fmt;

use serde_json::{Map, Value};

/// A parsed swaync `config.json` that can read and rewrite top-level values and
/// re-emit itself as canonical two-space pretty JSON.
///
/// Built by [`SwayncConfigFile::parse`]. Internally it holds the top-level JSON
/// object as a `serde_json` map, which (with the crate's `preserve_order`
/// feature) preserves key insertion order — so [`emit`](Self::emit) reproduces
/// the file's key order and, for a file already in canonical form, its exact
/// bytes.
#[derive(Clone, Debug)]
pub struct SwayncConfigFile {
    /// The config's top-level object. Storing the map directly (rather than a
    /// `Value`) encodes the invariant, established at [`parse`](Self::parse)
    /// time, that the root is an object — so the setters never have to re-check
    /// or fall back on a non-object root.
    root: Map<String, Value>,
}

/// A failure from [`SwayncConfigFile::parse`].
///
/// Parsing is all-or-nothing here (unlike the line-oriented parsers, which never
/// fail and instead collect per-line warnings): malformed JSON has no meaningful
/// partial representation, so it surfaces as an error rather than a panic (task
/// 3.4 acceptance).
#[derive(Debug)]
pub enum ParseError {
    /// The input is not syntactically valid JSON. Carries the underlying
    /// `serde_json` error, whose message includes the offending line and column.
    InvalidJson(serde_json::Error),
    /// The input is valid JSON but its top-level value is not an object (it is an
    /// array, string, number, boolean, or null). swaync's config is always a
    /// JSON object, and the whole get/set API addresses values by top-level key,
    /// so a non-object root has nothing to edit and is rejected.
    NotAnObject,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::InvalidJson(err) => write!(f, "swaync config is not valid JSON: {err}"),
            ParseError::NotAnObject => {
                write!(f, "swaync config's top-level JSON value is not an object")
            }
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ParseError::InvalidJson(err) => Some(err),
            ParseError::NotAnObject => None,
        }
    }
}

impl SwayncConfigFile {
    /// Parses swaync config text into an editable representation.
    ///
    /// Returns [`ParseError::InvalidJson`] if the text is not valid JSON and
    /// [`ParseError::NotAnObject`] if it parses to something other than a JSON
    /// object. Neither case panics (task 3.4 acceptance: "malformed JSON returns
    /// an error without panicking").
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let value: Value = serde_json::from_str(input).map_err(ParseError::InvalidJson)?;
        match value {
            Value::Object(root) => Ok(SwayncConfigFile { root }),
            _ => Err(ParseError::NotAnObject),
        }
    }

    /// Re-emits the config as canonical two-space pretty JSON with a trailing
    /// newline — the on-disk shape swaync uses.
    ///
    /// A file already in that canonical form round-trips byte-for-byte; a file in
    /// any other formatting is normalized to it (the expected formatting
    /// normalizations for JSON — see the module docs).
    ///
    /// This never panics. Serializing a `serde_json` `Value` built from parsed
    /// JSON is infallible — `serde_json` only errors here if a value's `Serialize`
    /// impl fails or a map has non-string keys, and a `Value` with a
    /// `Map<String, Value>` has neither. The `Err` branch is therefore
    /// unreachable in practice and exists only to keep `emit` total (panic-free):
    /// it logs the anomaly and falls back to compact JSON, which loses the pretty
    /// formatting but no data.
    pub fn emit(&self) -> String {
        let value = Value::Object(self.root.clone());
        match serde_json::to_string_pretty(&value) {
            Ok(mut pretty) => {
                // swaync writes a trailing newline after the closing brace; the
                // pretty printer omits it, so add it to match the on-disk format.
                pretty.push('\n');
                pretty
            }
            Err(err) => {
                tracing::error!(%err, "swaync config pretty serialization failed unexpectedly");
                let mut compact = value.to_string();
                compact.push('\n');
                compact
            }
        }
    }

    /// Returns the value of a top-level string key, or `None` if the key is
    /// absent or its value is not a string.
    ///
    /// Used for enum-like settings the Notifications page presents as a drop-down,
    /// e.g. `positionX` (`"left"`/`"right"`) and `positionY` (`"top"`/`"bottom"`).
    pub fn string(&self, key: &str) -> Option<&str> {
        self.root.get(key).and_then(Value::as_str)
    }

    /// Returns the value of a top-level integer key, or `None` if the key is
    /// absent or its value is not an integer representable as `i64`.
    ///
    /// Used for the notification timeout settings (`timeout`, `timeout-low`,
    /// `timeout-critical`), which are whole seconds.
    pub fn integer(&self, key: &str) -> Option<i64> {
        self.root.get(key).and_then(Value::as_i64)
    }

    /// Returns the value of a top-level boolean key, or `None` if the key is
    /// absent or its value is not a boolean.
    ///
    /// Used for the page's boolean on/off settings that are persisted as
    /// top-level flags in the config, e.g. `keyboard-shortcuts`, `fit-to-screen`,
    /// `notification-grouping`, or `hide-on-clear`.
    ///
    /// Caveat for task 6.7: swaync's do-not-disturb is **not** one of these — it
    /// is runtime daemon state toggled with `swaync-client`, and `dnd` appears in
    /// the config only as a `widgets` array entry and a `widget-config.dnd` label,
    /// not as a top-level boolean. Setting `dnd` here would append a dead key
    /// swaync ignores, so the DND control must drive the runtime mechanism, not
    /// this accessor; confirm the mechanism before treating DND as a config key.
    pub fn boolean(&self, key: &str) -> Option<bool> {
        self.root.get(key).and_then(Value::as_bool)
    }

    /// Sets a top-level key to a string value.
    ///
    /// If the key already exists its value is replaced and its position in the
    /// key order is kept; a new key is appended after the existing keys. This
    /// holds for every setter below and follows `serde_json`'s (index-map-backed)
    /// insertion semantics under the `preserve_order` feature.
    pub fn set_string(&mut self, key: &str, value: &str) {
        self.root.insert(key.to_string(), Value::from(value));
        tracing::debug!(key, value, "set swaync string value");
    }

    /// Sets a top-level key to an integer value. See [`set_string`](Self::set_string)
    /// for the insert-vs-update ordering behavior.
    pub fn set_integer(&mut self, key: &str, value: i64) {
        self.root.insert(key.to_string(), Value::from(value));
        tracing::debug!(key, value, "set swaync integer value");
    }

    /// Sets a top-level key to a boolean value. See [`set_string`](Self::set_string)
    /// for the insert-vs-update ordering behavior.
    pub fn set_boolean(&mut self, key: &str, value: bool) {
        self.root.insert(key.to_string(), Value::from(value));
        tracing::debug!(key, value, "set swaync boolean value");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic `swaync/config.json` fixture in swaync's exact on-disk shape:
    /// a JSON object, two-space pretty indentation, and a trailing newline. It
    /// mixes the value kinds the Notifications page edits — enum-like strings
    /// (`positionX`/`positionY`), integer timeouts, booleans — and includes a
    /// nested object and an array so that key-order and nesting preservation are
    /// exercised too. Keys are drawn from the real config (analysis §4, §5).
    const SWAYNC_FIXTURE: &str = "\
{
  \"$schema\": \"/etc/xdg/swaync/configSchema.json\",
  \"positionX\": \"right\",
  \"positionY\": \"top\",
  \"control-center-margin-top\": 52,
  \"notification-2fa-action\": true,
  \"timeout\": 10,
  \"timeout-low\": 5,
  \"timeout-critical\": 0,
  \"fit-to-screen\": false,
  \"keyboard-shortcuts\": true,
  \"image-visibility\": \"when-available\",
  \"widgets\": [
    \"inhibitors\",
    \"title\",
    \"dnd\",
    \"notifications\"
  ],
  \"widget-config\": {
    \"dnd\": {
      \"text\": \"Do Not Disturb\"
    }
  }
}
";

    /// Extracts the top-level key sequence from canonical two-space pretty JSON by
    /// collecting the keys indented by exactly two spaces (nested keys carry four
    /// or more). This reads the *serialized text* directly, so an assertion on it
    /// proves the emitted byte stream — not just the in-memory map — carries the
    /// expected key order.
    fn top_level_keys(json: &str) -> Vec<String> {
        json.lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("  ")?;
                if rest.starts_with(' ') || !rest.starts_with('"') {
                    return None;
                }
                let after_open = &rest[1..];
                let close = after_open.find('"')?;
                Some(after_open[..close].to_string())
            })
            .collect()
    }

    #[test]
    fn no_op_round_trip_preserves_bytes_and_key_order() {
        // Task 3.4 acceptance: a parse -> (no change) -> serialize cycle keeps the
        // object key order and swaync's canonical formatting. The fixture is
        // already in canonical form, so this is byte-identity.
        let config = SwayncConfigFile::parse(SWAYNC_FIXTURE).expect("fixture is valid JSON");
        let emitted = config.emit();
        assert_eq!(
            emitted, SWAYNC_FIXTURE,
            "a no-op round-trip must reproduce the canonical file byte-for-byte"
        );

        // Assert the serialized key sequence matches the input's, read straight
        // from the emitted text.
        assert_eq!(
            top_level_keys(&emitted),
            top_level_keys(SWAYNC_FIXTURE),
            "top-level key order must be stable across a no-op round-trip"
        );
    }

    #[test]
    fn emitted_output_ends_with_exactly_one_trailing_newline() {
        // swaync writes a single trailing newline; the pretty printer omits it, so
        // this guards the newline the emitter adds back.
        let config = SwayncConfigFile::parse(SWAYNC_FIXTURE).expect("valid JSON");
        let emitted = config.emit();
        assert!(
            emitted.ends_with("}\n"),
            "must end with a closing brace + newline"
        );
        assert!(
            !emitted.ends_with("}\n\n"),
            "must not accumulate extra trailing newlines"
        );
    }

    #[test]
    fn position_and_timeout_edits_round_trip_and_touch_nothing_else() {
        // Task 3.4 acceptance: editing a position and a timeout reads back the new
        // value and leaves every other key/value — and the key order — unchanged.
        let mut config = SwayncConfigFile::parse(SWAYNC_FIXTURE).expect("valid JSON");
        config.set_string("positionX", "left");
        config.set_integer("timeout", 30);

        // The new values read back through the typed getters.
        assert_eq!(config.string("positionX"), Some("left"));
        assert_eq!(config.integer("timeout"), Some(30));

        // The whole emitted file equals the fixture with only those two value
        // spans changed — the strongest possible "nothing else moved" assertion,
        // covering surrounding keys, the nested object/array, and key order at
        // once. `"timeout": 10` is unique (the `":` distinguishes it from
        // `timeout-low`/`timeout-critical`), so the replacements are unambiguous.
        let expected = SWAYNC_FIXTURE
            .replace("\"positionX\": \"right\"", "\"positionX\": \"left\"")
            .replace("\"timeout\": 10", "\"timeout\": 30");
        assert_eq!(
            config.emit(),
            expected,
            "only the edited values may change; all other bytes and the key order stay put"
        );

        // Spot-check a few untouched neighbors explicitly for clarity.
        assert_eq!(config.string("positionY"), Some("top"));
        assert_eq!(config.integer("timeout-low"), Some(5));
        assert_eq!(config.integer("timeout-critical"), Some(0));
        assert_eq!(config.boolean("keyboard-shortcuts"), Some(true));
    }

    #[test]
    fn boolean_edit_round_trips() {
        // A top-level boolean toggle (e.g. `fit-to-screen`) reads back the flipped
        // value and reserializes it. (swaync's do-not-disturb is runtime state via
        // `swaync-client`, not a config boolean — see `boolean`'s docs.)
        let mut config = SwayncConfigFile::parse(SWAYNC_FIXTURE).expect("valid JSON");
        assert_eq!(config.boolean("fit-to-screen"), Some(false));
        config.set_boolean("fit-to-screen", true);
        assert_eq!(config.boolean("fit-to-screen"), Some(true));

        let reparsed = SwayncConfigFile::parse(&config.emit()).expect("emit stays valid JSON");
        assert_eq!(reparsed.boolean("fit-to-screen"), Some(true));
    }

    #[test]
    fn updating_an_existing_key_keeps_its_position_setting_a_new_key_appends() {
        // Updating an existing key must not move it; a brand-new key is appended
        // after the existing ones (serde_json index-map insert semantics).
        let mut config = SwayncConfigFile::parse(SWAYNC_FIXTURE).expect("valid JSON");
        let original_order = top_level_keys(SWAYNC_FIXTURE);

        // Update in place: order unchanged.
        config.set_string("positionY", "bottom");
        assert_eq!(
            top_level_keys(&config.emit()),
            original_order,
            "updating an existing key must not reorder the object"
        );

        // Insert a new key: appended at the end.
        config.set_integer("transition-time", 250);
        let mut expected_order = original_order.clone();
        expected_order.push("transition-time".to_string());
        assert_eq!(
            top_level_keys(&config.emit()),
            expected_order,
            "a new key must be appended after the existing keys"
        );
    }

    #[test]
    fn typed_getters_return_none_for_absent_or_wrongly_typed_keys() {
        let config = SwayncConfigFile::parse(SWAYNC_FIXTURE).expect("valid JSON");

        // Absent key.
        assert_eq!(config.string("does-not-exist"), None);
        assert_eq!(config.integer("does-not-exist"), None);
        assert_eq!(config.boolean("does-not-exist"), None);

        // Present but wrong type: `positionX` is a string, `timeout` an integer.
        assert_eq!(config.integer("positionX"), None);
        assert_eq!(config.boolean("positionX"), None);
        assert_eq!(config.string("timeout"), None);
        assert_eq!(config.boolean("timeout"), None);
    }

    #[test]
    fn integer_returns_none_for_a_fractional_or_out_of_range_number() {
        // swaync timeouts are whole seconds; `integer()` is `as_i64`, which treats
        // a fractional number (`10.0`) and any value outside i64's range as "not an
        // integer". Documenting this guards against silently reading `10.0` back as
        // `10` or truncating an oversized value. (`1e20` overflows both i64 and u64,
        // so serde_json stores it as an f64, which `as_i64` rejects.)
        let config =
            SwayncConfigFile::parse("{ \"frac\": 10.0, \"huge\": 1e20 }").expect("valid JSON");
        assert_eq!(
            config.integer("frac"),
            None,
            "a fractional number is not an i64"
        );
        assert_eq!(
            config.integer("huge"),
            None,
            "a number beyond i64's range is not an i64"
        );
    }

    #[test]
    fn malformed_json_returns_an_error_without_panicking() {
        // Task 3.4 acceptance: malformed JSON surfaces as an error, never a panic.
        // A trailing comma is invalid JSON.
        let err = SwayncConfigFile::parse("{ \"positionX\": \"right\", }")
            .expect_err("a trailing comma is invalid JSON");
        assert!(matches!(err, ParseError::InvalidJson(_)));
        // The Display impl carries the underlying diagnostic without panicking.
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn valid_json_that_is_not_an_object_is_rejected() {
        // The get/set API addresses values by top-level key, so a non-object root
        // (here a JSON array) has nothing to edit and is rejected — but as an
        // error, not a panic.
        let err =
            SwayncConfigFile::parse("[1, 2, 3]").expect_err("root is an array, not an object");
        assert!(matches!(err, ParseError::NotAnObject));
    }

    #[test]
    fn compact_input_is_normalized_to_canonical_pretty_form() {
        // A file not already in canonical form (here compact, no trailing newline)
        // is normalized on emit: two-space pretty plus a trailing newline. This is
        // one of the expected JSON formatting normalizations (module docs).
        let compact = "{\"positionX\":\"right\",\"timeout\":10}";
        let config = SwayncConfigFile::parse(compact).expect("valid JSON");
        assert_eq!(
            config.emit(),
            "{\n  \"positionX\": \"right\",\n  \"timeout\": 10\n}\n"
        );
    }

    #[test]
    fn string_escapes_and_non_ascii_survive_a_round_trip() {
        // A string value carrying JSON-escaped characters (an embedded quote and a
        // backslash) plus raw non-ASCII must round-trip byte-for-byte. serde_json
        // escapes `"`/`\` canonically and leaves printable non-ASCII as raw UTF-8,
        // which is exactly the form used here, so parse -> emit reproduces it and
        // no edit is needed to prove escapes are preserved.
        //
        // The Rust literal below encodes this JSON text (value: a "quote" and a
        // backslash, then `café ☕`):
        //   {
        //     "text-empty": "a \"quote\" and a \\ and café ☕"
        //   }
        let fixture = "{\n  \"text-empty\": \"a \\\"quote\\\" and a \\\\ and café ☕\"\n}\n";
        let config = SwayncConfigFile::parse(fixture).expect("valid JSON");

        // The decoded value has the escapes resolved and the non-ASCII intact.
        assert_eq!(
            config.string("text-empty"),
            Some("a \"quote\" and a \\ and café ☕")
        );

        // And re-emitting reproduces the canonical escaped form byte-for-byte.
        assert_eq!(config.emit(), fixture);
    }
}
