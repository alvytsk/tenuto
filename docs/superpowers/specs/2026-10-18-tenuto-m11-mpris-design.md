# Tenuto M11: desktop media controls (MPRIS)

Status: approved 2026-10-18.

Branch: `feat/m11-mpris`, from `main` at `8875817`.

## 1. Intent

The user keeps tenuto running in a terminal while working in other windows. Their desktop (niri, Wayland) has a top-bar widget that lists media players over MPRIS2, the D-Bus interface every Linux bar, `playerctl` and GNOME/KDE read. cliamp already appears there. Tenuto should too: the widget shows **title**, **artist** and a working **play/pause button**, for both `tenuto tui` and `tenuto play`. The user clicks the widget; they do not use media keys or a progress bar.

## 2. Goals, non-goals, anti-goals

Goals:

- While `tui` or `play` runs on Linux, tenuto is an MPRIS2 player on the session bus.
- The widget shows the current title and artist (§5.1) and the right playing/paused state.
- Play/pause from the widget acts exactly like the space key.
- When tenuto exits, it disappears from the widget.

Non-goals (the interface still answers where MPRIS requires it, §5.3):

- Cover art (`mpris:artUrl`), album, `xesam:url`.
- The optional TrackList and Playlists interfaces; LoopStatus and Shuffle.
- OpenUri, Raise, Quit, Rate changes.
- Next/Previous and seeking from the widget, and the `Seeked` signal (§5.3). Tenuto's Next/Previous start the destination even while paused, and a seek past the end clamps, while MPRIS requires the paused state kept and a seek past the end to go to the next track. Matching that contract is extra work for controls the user doesn't use.
- macOS Now Playing and Windows SMTC. Other platforms build unchanged and have no media controls.

Anti-goals (failure even if the widget works):

- Tenuto fails to start, prints to the terminal, or stalls when there is no session bus (SSH, headless CI).
- A slow or hung D-Bus call delays a frame, a key, or quitting.
- Anything plays, loads or fetches without a click from the user.
- URLs or full paths leak onto the bus.
- Running the test suites registers players on the developer's desktop.

## 3. Constraints

- Linux only, pure-Rust D-Bus: no `libdbus-dev` build dependency, CI unchanged.
- Repo rules (`CLAUDE.md`): no `unsafe`/`unwrap`/`expect` in runtime code; layering per `docs/architecture.md` §4 and `tests/m9_layering.rs`; every URL in a log goes through `redact_url`; `--locked`, toolchain 1.98.1.
- Stopping or recreating the transport must not reset the logical position; MPRIS only sends `AppCommand`s, so this holds for free.

## 4. Structure

### 4.1 Dependency (my call)

`mpris-server = { version = "0.10", features = ["tokio"] }` in a new `[target.'cfg(target_os = "linux")'.dependencies]` section. It is current (2026-04), sits on zbus 5, and runs on Tokio, which tenuto already ships. `souvlaki` was rejected: last release 2025-06, still on zbus 3, and libdbus by default.

The builder checks `cargo tree -e features -i zbus` and turns off zbus's default runtime if the crate allows it. If a second async runtime (async-io) still comes in, note the size cost in the PR. It isn't a blocker.

### 4.2 Module and ownership (my call)

`src/application/mpris.rs`, inside `application` (rank 3), so `LAYERS` does not change. `PlayerRuntime` holds an optional handle to it:

- The front ends turn it on with one call after startup (e.g. `runtime.enable_media_controls()`; the name is the builder's choice). Both `tui` and `play` call it; nothing else does, so runtime suites never touch D-Bus.
- `pump()` drains the incoming requests at the start of each pass and feeds them through the same path as `runtime.handle(cmd)`. After applying events, it publishes the current state (§5). Neither front-end loop needs any other change.
- Latency is the existing wake interval: at most 50 ms in `tui` (`INPUT_POLL`), 100 ms in `play` (`next_key`'s budget). That's fine for a click.

### 4.3 Thread (my call)

A new thread, `tenuto-mpris`, owns the bus connection and its own current-thread Tokio runtime. It is separate from `HttpService`'s runtime because `play` may never start HTTP, and a stuck bus call must never stall a fetch.

- Requests to the runtime go over a small bounded crossbeam channel. The thread uses `try_send` and drops a request when the channel is full (only a click flood fills it).
- State to the thread is latest-value-wins. The mechanism is up to the builder; the runtime never blocks on it.
- Shutdown: dropping the handle disconnects the channels, the thread exits and the bus releases the name. The runtime never waits on the thread without a bound, so quitting never waits on D-Bus.
- A panic or bus error on the thread is contained: one log line, media controls are gone for the session, and playback carries on.
- A dropped bus connection doesn't come back, so stop publishing once it's gone instead of logging every failed emit. cliamp's `publish` once panicked here and left the terminal in raw mode.
- D-Bus method handlers never wait on the runtime; they only queue the request. cliamp hit a deadlock when a handler waited for its event loop while holding the properties lock.

### 4.4 Turning it off and failure

- `TENUTO_MPRIS=off` disables media controls. `tests/support` sets it for every subprocess suite. Document it in `reference.md` next to `TENUTO_AUDIO_OUTPUT`.
- The enable call spawns the thread and returns at once; it never waits for the bus. Connecting and authenticating happen on the thread, so a socket that accepts but never finishes the handshake can't delay startup, a frame or quitting.
- Connecting fails: tenuto runs without media controls and never writes to the terminal about it (my call).
  - The thread logs the failure itself, at a level the front end passes to the enable call.
  - `tui` passes `warn`; its fd 2 already goes to the log file.
  - `play` passes `debug`, because `telemetry::init` sends `info` and above to stderr there, and a line on every SSH run is noise.
- "No session bus" means whatever zbus's session-address lookup decides (my call). With `DBUS_SESSION_BUS_ADDRESS` unset, zbus falls back to `$XDG_RUNTIME_DIR/bus`, the same as every other player. So an SSH session into the desktop machine shows up in the bar. That's harmless: the controls only send what a key would.

### 4.5 Bus name

`org.mpris.MediaPlayer2.tenuto`. If the name is taken (a second profile running), use `org.mpris.MediaPlayer2.tenuto.instance<pid>`, the MPRIS convention for more than one instance.

## 5. What the widget sees

### 5.1 Metadata

Each `pump()` builds a small snapshot from `PlayerView` and hands it to the thread, every pass. The thread always updates its stored position, but emits `PropertiesChanged` only for the properties that changed. Position never counts as a change (§5.4).

| Key | Value |
|---|---|
| `mpris:trackid` | `/org/tenuto/track/<n>`, where `<n>` changes whenever the active `MediaId` changes; `/org/mpris/MediaPlayer2/TrackList/NoTrack` only when `view.now_playing` is `None` |
| `xesam:title` | per the table below |
| `xesam:artist` | a one-element array; omitted when there is no artist |
| `mpris:length` | microseconds, only when the media is finite and the duration is known; omitted for live streams (`view.live`) and unknown durations |

Nothing else goes on the bus: no URL, no path, no art.

Metadata that changes within the same media keeps the `trackid` and republishes `Metadata` (taken from cliamp's `sameTrack`). Examples: a probed duration arriving mid-play, or enrichment filling in tags.

Strings going onto the bus have NUL bytes stripped. D-Bus forbids them, and a tag can contain one; a NUL would make zbus refuse the whole `Metadata` message. Rust strings are already valid UTF-8.

An active entry that hasn't loaded yet still shows its title and artist, with status Stopped (my call). Examples: a restored entry at `tui` startup, a failed load, or a new entry whose load hasn't landed. This matches what the TUI itself draws from `NowPlaying` with `loaded: false`.

| Media | Title | Artist |
|---|---|---|
| Local file, `tui` | `NowPlaying.title` (tag title, else file name) | tag artist |
| Local file, `play` | decoder's title tag, else file name | decoder's artist tag |
| Podcast episode, `tui` | episode title | podcast name from the subscription; else decoder's artist tag; else none |
| Podcast episode, `play` | `NowPlaying.title` (decoder's title tag today) | decoder's artist tag; `play` has no library, so no podcast name |
| Radio station, either | what tenuto already shows (station name in `tui`, the URL's last segment in `play`) | none: tenuto refuses streams that carry per-song info |

Notes:

- **`play` gap (my call):** `detached_now_playing` (`runtime.rs`) sets only the title. It gains the decoder's title and artist tags. As a visible side effect, `play`'s status row then shows a local file's tag title instead of its file name, as `tui` does. This goes in the CHANGELOG.
- **Podcast name:** look it up from the subscription store only when the active media changes, never per pass. `podcast_artwork` (`application/podcast.rs`) shows the lookup.
- **Escaping:** the strings in `NowPlaying` are already escaped for the terminal. Reusing them is fine.

### 5.2 PlaybackStatus

| `PlaybackPhase` | MPRIS |
|---|---|
| Playing, Reconnecting | Playing |
| Loading | Playing. A load follows a user action. If the runtime can tell a load that will play from one that won't (e.g. startup restore), use that instead. |
| Paused | Paused |
| Unloaded, Stopped, Ended, LoadFailed | Stopped |

### 5.3 Properties

- Player:
  - `CanControl`, `CanPlay` and `CanPause` are always true. A request with nothing to act on becomes the runtime's usual notice or a no-op.
  - `CanGoNext`, `CanGoPrevious` and `CanSeek` are always false (§2 non-goals).
  - `Rate`, `MinimumRate` and `MaximumRate` are all 1.0.
  - `Volume` is `view.volume`'s gain (0.0–1.0).
  - If the crate requires `LoopStatus` and `Shuffle`, report `None` and false and ignore writes.
- Root:
  - `Identity` is `tenuto`. No `DesktopEntry`, since there is no `.desktop` file.
  - `CanQuit`, `CanRaise`, `CanSetFullscreen` and `HasTrackList` are false.
  - The supported URI schemes and MIME types are empty.

### 5.4 Position

`Position` returns the position from the latest pass, in microseconds, so it advances during ordinary playback. MPRIS defines it as never signalled, so a position-only update emits nothing. `Seeked` isn't emitted, since seeking is out of scope; a client that draws progress re-reads `Position` when it polls.

## 6. Requests to commands

| MPRIS | `AppCommand` |
|---|---|
| PlayPause | `PlayPause` |
| Play | `Play`, which already resumes a paused entry in both front ends (`transport.rs`: Play + Paused → `TransportDecision::Play`) |
| Pause | a new `AppCommand::Pause`, routed to the existing `PlaybackCommand::Pause`. `PlayPause` is not used: a toggle is resolved against the engine's newer state, so a Pause built from a stale snapshot could resume audio. The engine's `pause()` already does nothing unless it's playing. The builder confirms the indefinite branch (`pause_closing`) is equally harmless on a station that's already paused or stopped. With no engine, it's a no-op. |
| Stop | `Stop` |
| Volume = x | `AdjustVolume(clamp(x, 0, 1) − current gain)`; there is no absolute volume command. NaN is rejected with an invalid-argument error. After any Volume write, republish the actual `Volume`, even when it didn't change (clamped, or the engine was busy). Otherwise the client keeps showing the value it sent; cliamp fixed this as a bug. |
| Next, Previous, Seek, SetPosition, OpenUri, Raise, Quit, Rate, LoopStatus, Shuffle | nothing |

Every request is a user's click, so "nothing plays on its own" holds. MPRIS never fetches anything.

## 7. Tests

- **Pure mapping (unit, no D-Bus):**
  - view → snapshot, for each row of §5.1 and each phase of §5.2, plus live and unknown durations;
  - request → command for each row of §6, including the volume delta and the ignored methods;
  - change detection: a position-only update advances `Position` without emitting `PropertiesChanged`; a media change bumps `trackid`; a duration or tag change within the same media republishes `Metadata` with the same `trackid`;
  - a NUL in a tag is stripped; a NaN volume is rejected; a no-op volume write republishes `Volume`.
- **Service without a bus:** cliamp tests its service on a connection with a dummy transport (`newTestService`). Use zbus's in-process equivalent (a p2p connection over a socket pair) if one exists. Otherwise the mapping tests above cover the logic.
- **Runtime:** a request injected through a test-only constructor (not D-Bus) is applied by `pump()`. Wait with `pump_until`, per the runtime-suite rules.
- **Pause race:** a Pause request that arrives after the engine has paused, but before the runtime has seen the event, leaves it paused (engine level, on the virtual clock).
- **Unresponsive bus:** with the session address pointing at a Unix socket that accepts and never replies, the enable call returns at once, the runtime keeps pumping, and dropping it doesn't block.
- **D-Bus round trip (optional, the builder's call):** an `#[ignore]`d test under `dbus-run-session`, like `device_smoke`.
- **Subprocess suites** run with `TENUTO_MPRIS=off`.
- **Manual acceptance** (`docs/m11-acceptance.md`), in the user's niri bar, for both `tui` and `play`:
  - a tagged local track, a podcast episode and a radio station each show the right title and artist;
  - play/pause from the widget works and the widget's state follows the space key; Next/Previous don't appear;
  - the widget entry goes away on quit;
  - with `DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent`, tenuto starts normally and writes nothing to the terminal (`tui` logs one warning to its log file);
  - with `TENUTO_MPRIS=off`, tenuto doesn't appear in the widget.

## 8. Documentation

- `architecture.md`: add `application::mpris` to §4's table and a `tenuto-mpris` row to §5's thread table.
- `reference.md`: a short "Desktop media controls" section: what shows, what the button does, `TENUTO_MPRIS=off`, Linux only.
- `CHANGELOG.md` `[Unreleased]`: Added (MPRIS) and Changed (`play` shows tag titles).

## 9. Left to the builder

- `mpris-server`'s `Player` versus `Server<impl PlayerInterface>`.
- The latest-value channel, the method names, and the `trackid` counter.
- Whether to add the D-Bus round-trip test.
