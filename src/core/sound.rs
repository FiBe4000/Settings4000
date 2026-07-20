//! GTK-free Sound-page domain model (task 6.2; architecture §6 "Staging" + §7 Sound;
//! R3.1, R5.2, R6.2).
//!
//! # What this module is
//!
//! The Sound page is **entirely runtime-only** (R3.1/R5.2): PipeWire/WirePlumber keep
//! no dotfile in this setup (analysis §3), so every control applies *immediately* by
//! running a `wpctl` command — nothing is staged, nothing is dirty, and there is no
//! Apply/Reset involvement. This module is the headless half: it enumerates the current
//! audio devices and builds the exact `wpctl` argument vectors the controls run. The
//! bespoke GTK glue in [`crate::ui::sound`] renders the drop-downs/sliders/switches from
//! the enumerated [`SoundState`] and calls the command builders here on each change.
//!
//! Because it is runtime-only it deliberately does **not** touch the
//! [`SettingsStore`](crate::core::store): it holds no `SettingId`, stages nothing, and
//! never marks anything dirty. It stays GTK-free so the enumeration parsing and the
//! command building are unit-tested headlessly against a
//! [`MockCommandRunner`](crate::system::command::MockCommandRunner) (R6.2); the layering
//! guard in `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import here.
//!
//! # Enumeration: `pw-dump` JSON, falling back to `wpctl status`
//!
//! [`enumerate`] discovers the output (sink) and input (source) devices, which one is
//! the current default of each kind, and each device's volume and mute state. It first
//! runs `pw-dump` and parses the PipeWire object dump (the precise, structured source);
//! if `pw-dump` is absent, fails, or emits unparseable output it falls back to parsing
//! the human-readable `wpctl status`. If neither can be read it returns an empty
//! [`SoundState`] — it never panics on a missing or garbled source.
//!
//! # The cubic volume curve (a WirePlumber gotcha)
//!
//! WirePlumber stores a node's per-channel gain (`channelVolumes` in the `pw-dump`
//! `Props`) on a **cubic** curve, while `wpctl` presents the *linear* volume a user
//! expects: `wpctl set-volume ID V` stores `V³`, and `wpctl status`/`wpctl get-volume`
//! report `∛channelVolume`. This was confirmed empirically (`set-volume 0.5` yields a
//! stored `channelVolume` of `0.125`). So [`parse_pw_dump`] takes the cube root of the
//! stored channel volume to recover the `wpctl`-scale value shown on the slider, while
//! `wpctl status` already reports that scale and needs no conversion. The volume passed
//! to [`set_volume_command`] is the `wpctl`-scale value (`0.0`..=`1.0`), which `wpctl`
//! cubes on the way in — so read and write stay self-consistent.
//!
//! # The command set
//!
//! All three controls run `wpctl` with a node id resolved during enumeration:
//! [`set_default_command`] (`wpctl set-default ID`) switches the default output/input,
//! [`set_volume_command`] (`wpctl set-volume ID V`) sets a device's volume, and
//! [`set_mute_command`] (`wpctl set-mute ID 1|0`) mutes/unmutes it. They are built as
//! shell-free argument vectors and run through the [`CommandRunner`] seam, so a test can
//! assert the exact sequence without touching real audio.

use serde_json::Value as JsonValue;

use crate::system::command::{Command, CommandRunner};

/// The `media.class` a `pw-dump` node carries when it is an audio **output** (sink).
const AUDIO_SINK_CLASS: &str = "Audio/Sink";

/// The `media.class` a `pw-dump` node carries when it is an audio **input** (source).
///
/// Matched exactly so camera/video sources (`Video/Source`) and MIDI/monitor nodes are
/// never mistaken for microphones.
const AUDIO_SOURCE_CLASS: &str = "Audio/Source";

/// One audio endpoint as presented on the Sound page: a PipeWire node the user can make
/// default, adjust the volume of, or mute.
///
/// `volume` is the `wpctl`-scale linear volume (`0.0`..=`1.0`, `1.0` = 100%), i.e. the
/// value `wpctl set-volume` accepts and `wpctl status` reports — already cube-rooted
/// from the stored `channelVolumes` when read from `pw-dump` (see the module docs).
#[derive(Clone, Debug, PartialEq)]
pub struct SoundDevice {
    /// The PipeWire node id — the argument every `wpctl` control command targets.
    id: u32,
    /// The human-readable label shown in the drop-down (`node.description`, else
    /// `node.nick`, else `node.name`).
    label: String,
    /// Whether this is the current default device of its kind (the `*` in
    /// `wpctl status`, or the `default.audio.{sink,source}` metadata in `pw-dump`).
    is_default: bool,
    /// The device's `wpctl`-scale volume (`0.0`..=`1.0`).
    volume: f64,
    /// Whether the device is muted.
    muted: bool,
}

impl SoundDevice {
    /// The PipeWire node id (the `wpctl` command target).
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The human-readable label shown in the device drop-down.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether this device is the current default of its kind.
    pub fn is_default(&self) -> bool {
        self.is_default
    }

    /// The device's `wpctl`-scale volume (`0.0`..=`1.0`).
    pub fn volume(&self) -> f64 {
        self.volume
    }

    /// Whether the device is muted.
    pub fn muted(&self) -> bool {
        self.muted
    }
}

/// The enumerated PipeWire audio state: the output (sink) and input (source) devices,
/// each with its default flag, volume, and mute state.
///
/// Produced by [`enumerate`] on page entry (and on a manual rescan). A default value is
/// the empty state used when no audio source could be read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SoundState {
    /// The audio output (sink) devices, in enumeration order.
    outputs: Vec<SoundDevice>,
    /// The audio input (source/microphone) devices, in enumeration order.
    inputs: Vec<SoundDevice>,
}

impl SoundState {
    /// The audio output (sink) devices.
    pub fn outputs(&self) -> &[SoundDevice] {
        &self.outputs
    }

    /// The audio input (source/microphone) devices.
    pub fn inputs(&self) -> &[SoundDevice] {
        &self.inputs
    }
}

/// Enumerates the current audio devices, defaults, volumes, and mute states (R3.1).
///
/// Runs on page entry (and on a manual rescan). It prefers `pw-dump`'s structured JSON
/// and falls back to parsing `wpctl status` when `pw-dump` is unavailable or emits
/// output that cannot be parsed; if neither can be read it returns an empty
/// [`SoundState`]. It never panics — a missing binary, a non-zero exit, or garbled
/// output each degrade to the next source, then to empty.
pub fn enumerate(runner: &dyn CommandRunner) -> SoundState {
    if let Some(state) = enumerate_pw_dump(runner) {
        return state;
    }
    enumerate_wpctl_status(runner)
}

/// Runs `pw-dump` and parses it, or returns `None` so [`enumerate`] falls back.
///
/// `None` means the structured source could not be used at all — `pw-dump` is absent,
/// exited non-zero, or produced output that is not a JSON array. A successfully parsed
/// dump (even one with no audio nodes) returns `Some`, since that is an authoritative
/// "there are no audio devices" answer rather than a failure to read.
fn enumerate_pw_dump(runner: &dyn CommandRunner) -> Option<SoundState> {
    let output = match runner.run(&Command::new("pw-dump")) {
        Ok(output) if output.success() => output,
        Ok(_) => {
            tracing::info!("pw-dump exited non-zero; falling back to `wpctl status` (R3.1)");
            return None;
        }
        Err(error) => {
            tracing::info!(%error, "could not run pw-dump; falling back to `wpctl status` (R3.1)");
            return None;
        }
    };
    let json = String::from_utf8_lossy(output.stdout());
    match parse_pw_dump(&json) {
        Some(state) => Some(state),
        None => {
            tracing::info!("pw-dump output was not parseable JSON; falling back to `wpctl status`");
            None
        }
    }
}

/// Runs `wpctl status` and parses it, returning an empty [`SoundState`] on any failure.
///
/// This is the fallback when `pw-dump` is unusable. `wpctl status` is always parsed into
/// *some* state (possibly empty); a failure to run it at all degrades to the empty state
/// so the page renders "no devices" rather than erroring (R4.4-style degradation).
fn enumerate_wpctl_status(runner: &dyn CommandRunner) -> SoundState {
    match runner.run(&Command::new("wpctl").arg("status")) {
        Ok(output) if output.success() => {
            parse_wpctl_status(&String::from_utf8_lossy(output.stdout()))
        }
        Ok(_) => {
            tracing::info!("wpctl status exited non-zero; the Sound page has no device data");
            SoundState::default()
        }
        Err(error) => {
            tracing::info!(%error, "could not run wpctl status; the Sound page has no device data");
            SoundState::default()
        }
    }
}

/// Parses a `pw-dump` JSON array into the audio [`SoundState`] — the structured,
/// precise enumeration path, unit-tested against a canned dump (R6.2).
///
/// Returns `None` when the input is not a JSON array (so [`enumerate`] falls back to
/// `wpctl status`). Otherwise it walks the objects: the `default` metadata object names
/// the current default sink/source (by `node.name`), and each `Audio/Sink`/`Audio/Source`
/// node becomes a [`SoundDevice`] with its label, volume (cube-rooted from the stored
/// `channelVolumes`, see the module docs), mute state, and default flag. Video, MIDI, and
/// non-node objects are ignored. Malformed or partial objects are skipped rather than
/// panicking.
fn parse_pw_dump(json: &str) -> Option<SoundState> {
    let parsed: JsonValue = serde_json::from_str(json).ok()?;
    let objects = parsed.as_array()?;

    let (default_sink, default_source) = default_device_names(objects);
    let mut outputs = Vec::new();
    let mut inputs = Vec::new();

    for object in objects {
        if object.get("type").and_then(JsonValue::as_str) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(id) = object.get("id").and_then(JsonValue::as_u64) else {
            continue;
        };
        let Some(props) = object.get("info").and_then(|info| info.get("props")) else {
            continue;
        };
        let class = props
            .get("media.class")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        // Only plain audio sinks/sources are user-facing devices here; anything else
        // (video sources, MIDI bridges, monitor nodes) is not a Sound-page control.
        let (bucket, default_name) = match class {
            AUDIO_SINK_CLASS => (&mut outputs, default_sink.as_deref()),
            AUDIO_SOURCE_CLASS => (&mut inputs, default_source.as_deref()),
            _ => continue,
        };

        let node_name = props.get("node.name").and_then(JsonValue::as_str);
        let (volume, muted) = node_volume_and_mute(object);
        bucket.push(SoundDevice {
            id: id as u32,
            label: node_label(props),
            is_default: node_name.is_some() && node_name == default_name,
            volume,
            muted,
        });
    }

    Some(SoundState { outputs, inputs })
}

/// Finds the current default sink and source **node names** from the `default` metadata
/// object of a `pw-dump`.
///
/// PipeWire records the effective defaults under the metadata object whose
/// `metadata.name` is `default`, as `default.audio.sink` / `default.audio.source` keys
/// whose value is `{ "name": "<node.name>" }`. These are the *current* defaults (the `*`
/// in `wpctl status`), distinct from the persisted `default.configured.*` entries, so a
/// device is marked default only when it is actually the live default. Returns `(None,
/// None)` when there is no such metadata.
fn default_device_names(objects: &[JsonValue]) -> (Option<String>, Option<String>) {
    for object in objects {
        if object.get("type").and_then(JsonValue::as_str) != Some("PipeWire:Interface:Metadata") {
            continue;
        }
        if object
            .get("props")
            .and_then(|props| props.get("metadata.name"))
            .and_then(JsonValue::as_str)
            != Some("default")
        {
            continue;
        }

        let mut sink = None;
        let mut source = None;
        if let Some(entries) = object.get("metadata").and_then(JsonValue::as_array) {
            for entry in entries {
                let key = entry
                    .get("key")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                let name = entry
                    .get("value")
                    .and_then(|value| value.get("name"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_string);
                match key {
                    "default.audio.sink" => sink = name,
                    "default.audio.source" => source = name,
                    _ => {}
                }
            }
        }
        return (sink, source);
    }
    (None, None)
}

/// The human-readable label for a node, preferring `node.description`, then `node.nick`,
/// then `node.name`, then a generic fallback — never an empty string.
fn node_label(props: &JsonValue) -> String {
    for key in ["node.description", "node.nick", "node.name"] {
        if let Some(value) = props.get(key).and_then(JsonValue::as_str) {
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    "Unknown device".to_string()
}

/// Reads a node's `wpctl`-scale volume and mute state from its `pw-dump` `Props` param.
///
/// The volume is recovered by cube-rooting the stored `channelVolumes`, since
/// WirePlumber stores gain on a cubic curve (see the module docs), falling back to the
/// master `volume` when `channelVolumes` is absent. For a multi-channel device the
/// **first** channel is taken as the representative level (a deliberate choice over
/// averaging): the Sound page's own `wpctl set-volume` sets every channel to the same
/// value, so the channels normally match, and taking the first is stable and matches
/// how `wpctl` presents a single figure — a device left with per-channel imbalance by
/// an external tool simply shows its first channel here. A node whose `Props` are absent
/// or carry neither field defaults to full volume, unmuted — the neutral assumption for
/// a device we cannot read a level from.
fn node_volume_and_mute(node: &JsonValue) -> (f64, bool) {
    let props = node
        .get("info")
        .and_then(|info| info.get("params"))
        .and_then(|params| params.get("Props"))
        .and_then(JsonValue::as_array);
    let Some(props) = props else {
        return (1.0, false);
    };

    for entry in props {
        let channel = entry
            .get("channelVolumes")
            .and_then(JsonValue::as_array)
            .and_then(|volumes| volumes.first())
            .and_then(JsonValue::as_f64);
        let raw = channel.or_else(|| entry.get("volume").and_then(JsonValue::as_f64));
        if let Some(raw) = raw {
            let muted = entry
                .get("mute")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            return (cubic_to_linear(raw), muted);
        }
    }
    (1.0, false)
}

/// Converts a stored (cubic) `channelVolume` to the `wpctl`-scale linear volume shown to
/// the user, clamped to `0.0`..=`1.0`.
fn cubic_to_linear(stored: f64) -> f64 {
    stored.max(0.0).cbrt().clamp(0.0, 1.0)
}

/// Parses `wpctl status` text into the audio [`SoundState`] — the fallback enumeration
/// path when `pw-dump` is unusable, unit-tested against a canned status dump (R6.2).
///
/// `wpctl status` is a tree: top-level `Audio`/`Video`/`Settings` sections, each with
/// `Devices`/`Sinks`/`Sources`/… subsections. Only the **Audio** section's **Sinks**
/// (outputs) and **Sources** (inputs) node lines are parsed, so camera video sources and
/// the numeric device/config-default lists are ignored. Each node line carries an
/// optional `*` default marker, a numeric node id, a label, and a trailing
/// `[vol: N.NN[ MUTED]]`. A line that does not fit is skipped, never panicked on.
fn parse_wpctl_status(text: &str) -> SoundState {
    let mut in_audio = false;
    let mut subsection = Subsection::Ignore;
    let mut outputs = Vec::new();
    let mut inputs = Vec::new();

    for line in text.lines() {
        let Some(first) = line.chars().next() else {
            continue;
        };
        // A column-0 line is a top-level section header (`Audio`, `Video`, `Settings`,
        // or the `PipeWire …` banner). Switching section resets the subsection.
        if !first.is_whitespace() {
            in_audio = line.trim() == "Audio";
            subsection = Subsection::Ignore;
            continue;
        }

        let content = strip_tree_prefix(line);
        if let Some(sub) = subsection_of(content) {
            subsection = sub;
            continue;
        }

        if in_audio {
            match subsection {
                Subsection::Sinks => {
                    if let Some(device) = parse_status_node_line(content) {
                        outputs.push(device);
                    }
                }
                Subsection::Sources => {
                    if let Some(device) = parse_status_node_line(content) {
                        inputs.push(device);
                    }
                }
                Subsection::Ignore => {}
            }
        }
    }

    SoundState { outputs, inputs }
}

/// Which `wpctl status` subsection the parser is currently inside; only `Sinks` and
/// `Sources` yield devices.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Subsection {
    /// Audio output devices.
    Sinks,
    /// Audio input devices.
    Sources,
    /// Any other subsection (Devices/Filters/Streams/Clients/…), whose lines are skipped.
    Ignore,
}

/// Strips the leading tree-drawing prefix (whitespace and the box characters
/// `│ ├ └ ─`) `wpctl status` indents each line with, leaving the meaningful content.
fn strip_tree_prefix(line: &str) -> &str {
    line.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '├' | '└' | '─'))
}

/// Classifies a tree-stripped line as a subsection header, or `None` when it is a node
/// line (or anything else). `Sinks:`/`Sources:` select the audio buckets; the other
/// known headers reset to [`Subsection::Ignore`] so their node lists are skipped.
fn subsection_of(content: &str) -> Option<Subsection> {
    let content = content.trim();
    if content.starts_with("Sinks:") {
        Some(Subsection::Sinks)
    } else if content.starts_with("Sources:") {
        Some(Subsection::Sources)
    } else if content.starts_with("Devices:")
        || content.starts_with("Filters:")
        || content.starts_with("Streams:")
        || content.starts_with("Clients:")
    {
        Some(Subsection::Ignore)
    } else {
        None
    }
}

/// Parses one tree-stripped `wpctl status` node line into a [`SoundDevice`], or `None`
/// when it does not have the expected `[*] <id>. <label> [vol: N.NN[ MUTED]]` shape.
fn parse_status_node_line(content: &str) -> Option<SoundDevice> {
    let content = content.trim();
    let is_default = content.starts_with('*');
    let rest = content.trim_start_matches('*').trim_start();

    let (id_str, after) = rest.split_once('.')?;
    let id: u32 = id_str.trim().parse().ok()?;
    let after = after.trim_start();

    // The volume/mute live in a trailing `[…]`; the label is everything before it.
    let (label, volume, muted) = match after.rfind('[') {
        Some(open) => {
            let (volume, muted) = parse_status_volume(&after[open..]);
            (after[..open].trim().to_string(), volume, muted)
        }
        None => (after.trim().to_string(), 1.0, false),
    };

    Some(SoundDevice {
        id,
        label,
        is_default,
        volume,
        muted,
    })
}

/// Extracts the volume and mute state from a `wpctl status` `[vol: N.NN[ MUTED]]`
/// bracket. A missing/unparseable volume defaults to full, unmuted.
fn parse_status_volume(bracket: &str) -> (f64, bool) {
    let muted = bracket.contains("MUTED");
    let volume = bracket
        .find("vol:")
        .and_then(|at| bracket[at + "vol:".len()..].split_whitespace().next())
        .and_then(|token| token.trim_end_matches(']').parse::<f64>().ok())
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    (volume, muted)
}

/// Builds the `wpctl set-default ID` command that switches the default output/input to
/// the node `id` (R3.1/R5.2). `wpctl` infers the kind (playback/capture) from the node.
pub fn set_default_command(id: u32) -> Command {
    Command::new("wpctl").arg("set-default").arg(id.to_string())
}

/// Builds the `wpctl set-volume ID V` command that sets node `id`'s volume, where
/// `volume` is the `wpctl`-scale linear value (`0.0`..=`1.0`, clamped).
///
/// `wpctl` applies the cubic curve itself (see the module docs), so the value passed
/// here is exactly what the slider shows.
pub fn set_volume_command(id: u32, volume: f64) -> Command {
    let clamped = volume.clamp(0.0, 1.0);
    Command::new("wpctl")
        .arg("set-volume")
        .arg(id.to_string())
        .arg(format!("{clamped:.2}"))
}

/// Builds the `wpctl set-mute ID 1|0` command that mutes (`true`) or unmutes (`false`)
/// node `id`.
pub fn set_mute_command(id: u32, muted: bool) -> Command {
    Command::new("wpctl")
        .arg("set-mute")
        .arg(id.to_string())
        .arg(if muted { "1" } else { "0" })
}

/// Immediately switches the default device to node `id` (R5.2), logging the outcome.
pub fn set_default(runner: &dyn CommandRunner, id: u32) {
    run_control(runner, set_default_command(id));
}

/// Immediately sets node `id`'s volume to the `wpctl`-scale `volume` (R5.2).
pub fn set_volume(runner: &dyn CommandRunner, id: u32, volume: f64) {
    run_control(runner, set_volume_command(id, volume));
}

/// Immediately mutes/unmutes node `id` (R5.2).
pub fn set_mute(runner: &dyn CommandRunner, id: u32, muted: bool) {
    run_control(runner, set_mute_command(id, muted));
}

/// Runs a runtime `wpctl` control command, logging success at `info` and any failure at
/// `error` (non-fatal, R5.5-style — a failed runtime control changes nothing on disk and
/// simply leaves the audio state as it was).
fn run_control(runner: &dyn CommandRunner, command: Command) {
    match runner.run(&command) {
        Ok(output) if output.success() => {
            tracing::info!(%command, "applied sound control (runtime-only, R5.2)");
        }
        Ok(output) => {
            tracing::error!(%command, code = ?output.code(), "sound control command failed");
        }
        Err(error) => {
            tracing::error!(%command, %error, "could not run sound control command");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::command::{CommandOutput, MockCommandRunner};

    /// Two output sinks (one default, one muted), one default input source, plus a video
    /// source and a non-node object that must be ignored. Volumes are the stored cubic
    /// `channelVolumes`, so the parser must cube-root them: `0.125 → 0.5`, `1.0 → 1.0`,
    /// `0.216 → 0.6`.
    const PW_DUMP: &str = r#"[
        {
            "id": 39,
            "type": "PipeWire:Interface:Metadata",
            "props": { "metadata.name": "default" },
            "metadata": [
                { "key": "default.audio.sink", "value": { "name": "sink-a" } },
                { "key": "default.audio.source", "value": { "name": "source-a" } },
                { "key": "default.configured.audio.sink", "value": { "name": "sink-b" } }
            ]
        },
        {
            "id": 40,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Audio/Sink",
                    "node.name": "sink-a",
                    "node.description": "Speakers"
                },
                "params": { "Props": [ { "volume": 1.0, "mute": false, "channelVolumes": [0.125, 0.125] } ] }
            }
        },
        {
            "id": 41,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Audio/Sink",
                    "node.name": "sink-b",
                    "node.description": "HDMI"
                },
                "params": { "Props": [ { "volume": 1.0, "mute": true, "channelVolumes": [1.0, 1.0] } ] }
            }
        },
        {
            "id": 50,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Audio/Source",
                    "node.name": "source-a",
                    "node.description": "Microphone"
                },
                "params": { "Props": [ { "volume": 1.0, "mute": false, "channelVolumes": [0.216, 0.216] } ] }
            }
        },
        {
            "id": 60,
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "media.class": "Video/Source",
                    "node.name": "camera",
                    "node.description": "Integrated Camera"
                }
            }
        },
        { "id": 1, "type": "PipeWire:Interface:Client" }
    ]"#;

    /// A realistic `wpctl status` dump: an `Audio` section with two sinks (one default,
    /// one muted) and one source, a `Video` section whose source must be ignored, and a
    /// `Settings` section whose numeric config-default lines must be ignored.
    const WPCTL_STATUS: &str = "PipeWire 'pipewire-0' [1.6.7, filip@host, cookie:1]
 \u{2514}\u{2500} Clients:
        33. WirePlumber                         [1.6.7]

Audio
 \u{251c}\u{2500} Devices:
 \u{2502}      43. Built-in Audio                      [alsa]
 \u{2502}
 \u{251c}\u{2500} Sinks:
 \u{2502}  *   67. Speakers                            [vol: 0.35]
 \u{2502}      68. HDMI                                [vol: 1.00 MUTED]
 \u{2502}
 \u{251c}\u{2500} Sources:
 \u{2502}  *   70. Microphone                          [vol: 0.80]
 \u{2502}
 \u{2514}\u{2500} Streams:

Video
 \u{251c}\u{2500} Sources:
 \u{2502}  *   58. Integrated Camera (V4L2)
 \u{2514}\u{2500} Streams:

Settings
 \u{2514}\u{2500} Default Configured Devices:
         0. Audio/Sink    alsa_output.some-dock.analog-stereo
         1. Audio/Source  alsa_input.some-dock.mono-fallback
";

    /// Compares a `wpctl`-scale volume with the tolerance a cube-root round-trip needs.
    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected ~{expected}, got {actual}"
        );
    }

    #[test]
    fn parse_pw_dump_reads_devices_defaults_volume_and_mute() {
        let state = parse_pw_dump(PW_DUMP).expect("valid JSON array parses");

        // Two outputs; the first is default, the second muted at full volume. Video and
        // non-node objects are excluded.
        assert_eq!(state.outputs().len(), 2, "only the two Audio/Sink nodes");
        let speakers = &state.outputs()[0];
        assert_eq!(speakers.id(), 40);
        assert_eq!(speakers.label(), "Speakers");
        assert!(speakers.is_default(), "sink-a is the default sink");
        approx(speakers.volume(), 0.5); // cube root of the stored 0.125
        assert!(!speakers.muted());

        let hdmi = &state.outputs()[1];
        assert_eq!(hdmi.id(), 41);
        assert!(
            !hdmi.is_default(),
            "the configured (not current) default is ignored"
        );
        approx(hdmi.volume(), 1.0);
        assert!(hdmi.muted());

        // One input, marked default, cube-rooted from 0.216 to 0.6.
        assert_eq!(state.inputs().len(), 1, "only the Audio/Source node");
        let mic = &state.inputs()[0];
        assert_eq!(mic.id(), 50);
        assert_eq!(mic.label(), "Microphone");
        assert!(mic.is_default());
        approx(mic.volume(), 0.6);
        assert!(!mic.muted());
    }

    #[test]
    fn parse_pw_dump_uses_the_first_channel_for_unequal_multichannel_volumes() {
        // N2: with per-channel imbalance the FIRST channel is the representative level.
        // channelVolumes[0] = 0.125 -> 0.5; the second (0.729 -> 0.9) is ignored.
        let json = r#"[
            {
                "id": 40,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": { "media.class": "Audio/Sink", "node.name": "s", "node.description": "S" },
                    "params": { "Props": [ { "mute": false, "channelVolumes": [0.125, 0.729] } ] }
                }
            }
        ]"#;
        let state = parse_pw_dump(json).expect("valid JSON array");
        assert_eq!(state.outputs().len(), 1);
        approx(state.outputs()[0].volume(), 0.5);
    }

    #[test]
    fn parse_pw_dump_node_without_props_defaults_to_full_unmuted() {
        // A node with no `params`/`Props` (e.g. restricted permissions) must not panic
        // and defaults to full volume, unmuted.
        let json = r#"[
            {
                "id": 41,
                "type": "PipeWire:Interface:Node",
                "info": { "props": { "media.class": "Audio/Sink", "node.name": "x", "node.description": "X" } }
            }
        ]"#;
        let state = parse_pw_dump(json).expect("valid JSON array");
        assert_eq!(state.outputs().len(), 1);
        approx(state.outputs()[0].volume(), 1.0);
        assert!(!state.outputs()[0].muted());
    }

    #[test]
    fn parse_pw_dump_with_zero_audio_nodes_returns_some_empty() {
        // A valid but audio-free dump is an authoritative "no devices", not a parse
        // failure — so it returns Some(empty) rather than None.
        assert_eq!(parse_pw_dump("[]"), Some(SoundState::default()));
    }

    #[test]
    fn enumerate_does_not_fall_back_when_pw_dump_reports_no_audio() {
        // The authoritative empty answer must NOT trigger the wpctl status fallback:
        // only pw-dump is run.
        let runner =
            MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(0, "[]", ""))]);
        let state = enumerate(&runner);
        assert!(state.outputs().is_empty() && state.inputs().is_empty());
        assert_eq!(
            runner.recorded(),
            vec![Command::new("pw-dump")],
            "a valid empty pw-dump is authoritative; wpctl status is not consulted"
        );
    }

    #[test]
    fn parse_pw_dump_rejects_non_array_so_enumerate_falls_back() {
        // A non-array (or unparseable) dump yields None, which is what makes `enumerate`
        // fall back to `wpctl status`.
        assert!(parse_pw_dump("not json").is_none());
        assert!(
            parse_pw_dump("{}").is_none(),
            "a JSON object is not the dump array"
        );
    }

    #[test]
    fn parse_wpctl_status_reads_audio_sinks_and_sources_only() {
        let state = parse_wpctl_status(WPCTL_STATUS);

        // The Audio sinks — the Video source and the Settings config lines are excluded.
        assert_eq!(state.outputs().len(), 2);
        let speakers = &state.outputs()[0];
        assert_eq!(speakers.id(), 67);
        assert_eq!(speakers.label(), "Speakers");
        assert!(speakers.is_default());
        approx(speakers.volume(), 0.35);
        assert!(!speakers.muted());

        let hdmi = &state.outputs()[1];
        assert_eq!(hdmi.id(), 68);
        assert!(!hdmi.is_default());
        approx(hdmi.volume(), 1.0);
        assert!(hdmi.muted());

        // The single Audio source; the Video "Integrated Camera" source is not an input.
        assert_eq!(
            state.inputs().len(),
            1,
            "the camera Video/Source is excluded"
        );
        let mic = &state.inputs()[0];
        assert_eq!(mic.id(), 70);
        assert_eq!(mic.label(), "Microphone");
        assert!(mic.is_default());
        approx(mic.volume(), 0.8);
    }

    #[test]
    fn enumerate_prefers_pw_dump() {
        // With a parseable pw-dump, `enumerate` uses it and never runs `wpctl status`.
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0, PW_DUMP, "",
        ))]);
        let state = enumerate(&runner);
        assert_eq!(state.outputs().len(), 2);
        assert_eq!(state.inputs().len(), 1);
        assert_eq!(
            runner.recorded(),
            vec![Command::new("pw-dump")],
            "only pw-dump is run when it parses"
        );
    }

    #[test]
    fn enumerate_falls_back_to_wpctl_status_when_pw_dump_fails_to_run() {
        // pw-dump absent (spawn error) -> fall back to `wpctl status`.
        let runner = MockCommandRunner::with_outcomes([
            Err(crate::system::command::CommandError::Spawn(
                std::io::Error::from(std::io::ErrorKind::NotFound),
            )),
            Ok(CommandOutput::fake_with_streams(0, WPCTL_STATUS, "")),
        ]);
        let state = enumerate(&runner);
        assert_eq!(
            state.outputs().len(),
            2,
            "devices come from the wpctl status fallback"
        );
        assert_eq!(
            runner.recorded(),
            vec![Command::new("pw-dump"), Command::new("wpctl").arg("status"),],
            "pw-dump is tried first, then the wpctl status fallback"
        );
    }

    #[test]
    fn enumerate_falls_back_when_pw_dump_output_is_garbled() {
        // pw-dump runs but emits non-JSON -> fall back to `wpctl status`.
        let runner = MockCommandRunner::with_outcomes([
            Ok(CommandOutput::fake_with_streams(0, "<<< not json >>>", "")),
            Ok(CommandOutput::fake_with_streams(0, WPCTL_STATUS, "")),
        ]);
        let state = enumerate(&runner);
        assert_eq!(
            state.inputs().len(),
            1,
            "the fallback still enumerates the mic"
        );
        assert_eq!(
            runner.recorded(),
            vec![Command::new("pw-dump"), Command::new("wpctl").arg("status"),],
        );
    }

    #[test]
    fn enumerate_degrades_to_empty_when_both_sources_fail() {
        // Neither source usable -> empty state, no panic.
        let runner = MockCommandRunner::with_outcomes([
            Ok(CommandOutput::fake(1)),
            Ok(CommandOutput::fake(1)),
        ]);
        let state = enumerate(&runner);
        assert!(state.outputs().is_empty() && state.inputs().is_empty());
    }

    #[test]
    fn command_builders_emit_the_exact_wpctl_arg_vectors() {
        // The exact shell-free argument vectors each control runs (R5.2).
        assert_eq!(
            set_default_command(42),
            Command::new("wpctl").args(["set-default", "42"])
        );
        assert_eq!(
            set_volume_command(42, 0.5),
            Command::new("wpctl").args(["set-volume", "42", "0.50"]),
            "the wpctl-scale volume is passed through; wpctl cubes it"
        );
        // Out-of-range volumes are clamped before formatting.
        assert_eq!(
            set_volume_command(7, 1.7),
            Command::new("wpctl").args(["set-volume", "7", "1.00"])
        );
        assert_eq!(
            set_mute_command(42, true),
            Command::new("wpctl").args(["set-mute", "42", "1"])
        );
        assert_eq!(
            set_mute_command(42, false),
            Command::new("wpctl").args(["set-mute", "42", "0"])
        );
    }

    #[test]
    fn control_functions_issue_the_expected_commands_through_the_runner() {
        // Driving a control runs exactly the built command via the runner (R6.1) — the
        // proof that a device switch / volume / mute reaches `wpctl` with the right id.
        let runner = MockCommandRunner::new();
        set_default(&runner, 67);
        set_volume(&runner, 67, 0.25);
        set_mute(&runner, 70, true);
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("wpctl").args(["set-default", "67"]),
                Command::new("wpctl").args(["set-volume", "67", "0.25"]),
                Command::new("wpctl").args(["set-mute", "70", "1"]),
            ],
        );
    }
}
