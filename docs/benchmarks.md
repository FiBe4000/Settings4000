# Startup-time benchmark (task 7.3)

**Budget:** cold startup to interactive window **< 500 ms** on the target
hardware (R8.1).

CI has no Wayland compositor, so this number cannot be produced there; instead
the app carries permanent, zero-steady-state-overhead instrumentation and this
document defines the reproducible manual procedure. Results are recorded at the
bottom.

## What is measured

The binary instruments itself; no external timer or `GTK_DEBUG` tooling is
involved:

- **Start point:** a `std::time::Instant` captured as the *very first
  statement* of `main` (`src/main.rs`), before CLI parsing, logging setup, or
  GTK initialization. Only the pre-`main` runtime setup (dynamic linking,
  crt0) falls outside the window.
- **End point ("first frame"):** the GDK frame clock's **`after-paint`** phase
  of the main window's first frame cycle (`arm_first_frame_mark` in
  `src/ui/app.rs`). At that moment GTK has laid out, rendered, and handed the
  finished frame to the compositor — the closest point to "the window is on
  screen" an app can observe through GTK. The compositor's actual scan-out
  happens outside the process and is not synchronously observable (GDK only
  exposes the presentation timestamp post hoc, via `GdkFrameTimings`); earlier
  signals (`map`, the first tick) fire *before* the frame is rendered and
  would flatter the number.

When the first frame is painted the app logs, at `info`:

```
first frame painted <N> ms after process start (task 7.3 startup mark; R8.1 budget: 500 ms)
```

with the value also attached as the structured field `startup_ms` (journald
field `STARTUP_MS`). The mark is one-shot: the tick callback and the
`after-paint` handler both unregister themselves after firing, so nothing
remains connected in steady state.

**Placeholder vs. populated window:** architecture §8 runs detection and
config parsing on a worker thread, so the first frame is *nominally* the shell
with its loading placeholder — that immediately-interactive shell is what the
R8.1 budget targets. In practice the worker wins the race on the target
hardware: the adjacent `startup load complete` log line lands ~35–50 ms
*before* the first-frame mark, so the frame being measured is already the
fully populated window. Compare the two lines' journald timestamps to see the
populate latency explicitly.

## Manual procedure

1. Build the release binary (the budget applies to the optimized build):

   ```
   cargo build --release
   ```

2. Make sure no instance is already running — Settings4000 is single-instance
   (R8.4), and a relaunch would just activate the existing window and log no
   mark:

   ```
   pgrep -x settings4000   # must print nothing
   ```

3. From a live Hyprland/Wayland session, launch the app briefly and let
   `timeout` close it. Startup only *reads*: detection is a filesystem/procfs
   scan, and the only spawned commands are read-only probes —
   `hyprctl monitors all -j`, plus `swaync-client --get-dnd` when swaync is
   present — so opening and closing the window changes nothing. Just don't
   click **Apply**:

   ```
   timeout 3 ./target/release/settings4000
   ```

4. Read the mark. Logs go to journald when available, else stderr (R7.1):

   ```
   journalctl --user -t settings4000 --since "-1 minute" | grep "first frame painted"
   ```

5. Repeat for **5 runs** total (a 1 s pause between runs is plenty) and record
   every value plus the median.

### Cold vs. warm caveat

Consecutive runs are **warm-cache** starts: the binary, shared libraries, and
config files are in the page cache. A true cold-from-disk start requires
dropping caches first (`echo 3 | sudo tee /proc/sys/vm/drop_caches`), which
needs root and perturbs the whole session, so it is *not* part of the standard
procedure — record warm numbers and note them as such. The first run after a
fresh build (first `exec` of the new binary) is the coldest sample the
standard procedure produces; record it alongside the median rather than
discarding it as an outlier.

## Recorded measurements

| Date | Machine | Session | Runs (ms) | Median | Notes |
|------|---------|---------|-----------|--------|-------|
| 2026-07-20 | Intel i7-8550U, Arch Linux (kernel 7.1.2-zen3-1-zen) | Hyprland 0.55.4, Wayland | 371, 251, 253, 260, 249 | **253 ms** | Warm-cache runs right after `cargo build --release`; the 371 ms first run is the first `exec` of the fresh binary (coldest sample). All runs within the 500 ms budget; the startup worker finished 35–50 ms *before* the first frame in every run, so each measured frame was the fully populated window. |
