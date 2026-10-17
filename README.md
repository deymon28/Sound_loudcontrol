# LoudControl

Per-application audio ducking for Windows. When one app makes sound (for example a voice
call in Discord), LoudControl smoothly lowers the volume of other apps (for example your
music player), then brings it back when the sound stops.

Rules decide which apps trigger ducking, which apps get lowered, and how:

- **Triggers and targets** per rule: any process by executable name, Windows system sounds,
  or "all other apps".
- **Detection**: threshold in dBFS with a live meter, and a minimum duration that ignores
  clicks and notification blips.
- **Ducking**: fixed level (0–100 %, 0 mutes) or proportional mode, where louder trigger
  sound gives a deeper duck. Fades are decibel-smooth or linear.
- **Timing**: fade down (attack), hold (bridges pauses between words), and fade up (release),
  with presets.
- **Live view**: a scrolling graph of trigger level and target volume, plus meters for every
  app that is playing audio.
- Tray icon (it changes colour while ducking), a global pause hotkey (default
  `Ctrl+Alt+M`), start with Windows, start minimized.

## Requirements

- Windows 10 or 11.
- To build: a Rust toolchain (stable, edition 2024) with the MSVC target.

## Build and run

```powershell
cargo build --release
.\target\release\loudcontrol.exe
```

The executable is self-contained. Copy `target\release\loudcontrol.exe` anywhere and run it.

Command-line options:

| Option | Effect |
| --- | --- |
| `--minimized` | Start in the tray (this is what "Start with Windows" uses). |
| `--list` | Print every audio session with its executable name, volume, peak level and device, then exit. Use it to find the exact name to put in a rule. |

## How it works

LoudControl uses the Windows Core Audio session API (WASAPI). Every app that plays sound has
an *audio session*, which is the same thing the Windows volume mixer shows.

1. A dedicated engine thread reads each trigger session's peak meter every 10 ms (configurable)
   with a high-resolution waitable timer. When no trigger app is running, it polls at 10 Hz.
2. Each rule runs a small state machine: threshold → minimum duration → hold → attack/release
   envelope (see `src/dsp.rs`).
3. A target's session volume is set to *your volume × duck gain*. If a target is lowered by
   several rules, the deepest duck wins. LoudControl only touches a session while it is ducked.
   If you move that app's slider in the mixer meanwhile, your new value becomes the volume it
   returns to.

Volume is always handed back:

- on normal exit and when Windows signs out or shuts down, every ducked app is restored
  immediately;
- if the process is killed while something is ducked, the original volumes stay in
  `restore.toml`, and the next launch restores them (even after the app itself was restarted).

Session peak meters measure the signal before the session volume. Ducking a target therefore
never changes its own meter reading, and a trigger's mixer slider does not affect detection.

## Configuration

Settings are stored in `%APPDATA%\LoudControl\config.toml`. Changes made in the UI are saved
automatically. The file can also be edited by hand and reloaded from **Settings → Files**.
Set the `LOUDCONTROL_HOME` environment variable to use a different directory.

```toml
[settings]
paused = false
tick_ms = 10                 # meter poll interval while a trigger app runs (5–100)
pause_hotkey = "Ctrl+Alt+M"  # empty string disables it
start_minimized = false
close_to_tray = true

[[rules]]
name = "Discord over music"
enabled = true
triggers = [{ process = "Discord.exe" }]
targets = [{ process = "YouTube Music.exe" }]   # also: "system_sounds", "all_others"
threshold_db = -45.0         # trigger peak level that counts as sound
min_active_ms = 40.0         # ignore sounds shorter than this
mode = "fixed"               # or "proportional"
duck_level = 0.25            # target volume while ducked, fraction of normal (0 = mute)
range_db = 24.0              # proportional mode: dB above threshold for the full duck
curve = "decibel"            # or "linear"
attack_ms = 120.0
hold_ms = 900.0
release_ms = 1200.0
```

Logs go to `%APPDATA%\LoudControl\loudcontrol.log`, which rotates at 1 MB. Set
`LOUDCONTROL_LOG=debug` for per-session detail.

## Limitations

- Windows mixes audio per process, not per browser tab. If your music plays in a browser,
  ducking it lowers the whole browser. A desktop music client avoids this.
- Matching is by executable name. Apps that play audio through a helper process (some
  Electron and UWP apps) have to be matched by that helper's name. `--list` shows it.
- `taskkill` without `/F` cannot close the app gracefully while it sits in the tray. Use the
  tray menu's **Quit**. A forced kill is still covered by the restore-on-next-launch fallback.

## Development

```powershell
cargo test            # rule logic, config parsing
cargo clippy --release
```

Source layout:

| File | Responsibility |
| --- | --- |
| `src/audio.rs` | WASAPI devices, sessions, meters, volumes |
| `src/engine.rs` | Engine thread: rule evaluation, volume ownership, crash-safe restore |
| `src/dsp.rs` | Pure ducking logic (detector, envelope, gain curves) with unit tests |
| `src/config.rs` | TOML configuration model |
| `src/ui.rs` | egui interface |
| `src/tray.rs` | Tray icon, pause hotkey, second-launch activation |
| `src/system.rs` | Hidden window for shutdown / close messages |
| `src/icon_art.rs` | Procedural icon, shared with `build.rs` for the `.exe` icon |
