# Tenuto

[![CI](https://github.com/alvytsk/tenuto/actions/workflows/ci.yml/badge.svg)](https://github.com/alvytsk/tenuto/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/tenuto.svg)](https://crates.io/crates/tenuto)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://github.com/alvytsk/tenuto/blob/main/LICENSE)

A keyboard-first terminal audio player for local files, HTTP media and podcasts.

- Plays MP3, FLAC, WAV and M4A files, direct `http(s)://` URLs, and episodes of the podcasts you subscribe to.
- Remembers where you stopped in every track and episode, and resumes there next time.
- Never plays, fetches or refreshes anything on its own. Every network request follows a key you pressed or a command you ran.

![The terminal player: cover art, track information, spectrum, transport and a playlist](https://raw.githubusercontent.com/alvytsk/tenuto/main/docs/images/tui.webp)

## Install

```sh
cargo install tenuto
```

On Linux, install `libasound2-dev` first. CPAL needs the ALSA headers, and
the runtime `libasound.so.2` alone is not enough.

### Prebuilt packages (x86_64 Linux)

Each [GitHub Release](https://github.com/alvytsk/tenuto/releases) carries
a `.deb`, a tarball, `SHA256SUMS` and `build-info.txt`. Verify the download
first:

```sh
sha256sum -c SHA256SUMS --ignore-missing
```

**Debian and Ubuntu.** Tested on Debian 12 and 13; Ubuntu 22.04, 24.04 and 26.04.

```sh
sudo apt install ./tenuto_<version>-1_amd64.deb
```

**Tarball.** Built on Ubuntu 22.04 and tested on the same five releases. It
needs glibc 2.34 or newer, the ALSA runtime library (`libasound2`, or
`libasound2t64` on Debian 13 and Ubuntu 24.04 and later) and
`ca-certificates`. The binary links exactly `ld-linux-x86-64.so.2`,
`libasound.so.2`, `libc.so.6`, `libgcc_s.so.1` and `libm.so.6`.

```sh
tar -xzf tenuto-<version>-x86_64-unknown-linux-gnu.tar.gz
./tenuto-<version>-x86_64-unknown-linux-gnu/tenuto --version
```

Linux is the verified platform. The crate also builds on macOS and CI runs
the suite there, but one playback test still fails on macOS, so it is not
claimed as supported yet. Windows is neither built nor claimed.

### From source

1. Install Rust through rustup. The repository pins Rust 1.98.1 with the
   rustfmt and clippy components.
2. On Linux, install `libasound2-dev`.
3. Build from the repository root:

```sh
cargo build --release --locked
```

The binary is `target/release/tenuto`. The examples below assume it is on
your `PATH`.

## Your first ten minutes

**Play a file.** Point Tenuto at any MP3, FLAC, WAV or M4A:

```sh
tenuto play ~/Music/episode.mp3
```

The terminal shows one status line with the track name, the state, the position, the duration and the volume, and a help line under it. Space pauses, the arrow keys seek ten seconds, `q` quits.

**Come back later.** Quit halfway through and run the same command again. Playback resumes where you stopped. Tenuto remembers a position for every file, URL and episode it has played.

**Open the player.** Run `tenuto` with no arguments:

```sh
tenuto
```

The full-screen player opens on your playlists — one `Default` playlist, empty the first time — and nothing plays until you ask. Press `a`, type a path or an `http(s)://` URL, and press Enter to add it to the playlist you're viewing. Press Space to play. The keys you need most are listed along the bottom of the screen, and `?` shows all of them.

**Subscribe to a podcast.** Press `b` to open the browser, then Tab to switch to the Podcasts tab. Press `a`, paste the feed URL and press Enter. The feed is fetched once and its episodes appear in the list. Move to an episode and press Enter to add it to your playlist, then Space to play it. The same works from a shell:

```sh
tenuto subscribe http://feeds.rucast.net/radio-t --as radio-t
tenuto episodes radio-t -n 5
tenuto play radio-t 3
```

**Quit.** `q` or Ctrl-C saves your position and leaves the terminal as it found it.

## The player

The player restores every playlist, the playing one's active entry, the volume and every saved position. It never starts playing on its own, and nothing is fetched until you press a playback key. Cover art is read from the file's tag, from a `cover.jpg` or `folder.jpg` beside it, or for a podcast from the feed's own image.

| Key | Action |
|---|---|
| Space | Pause or resume. Before anything is loaded, resume the playing playlist's remembered entry, or its first track in playback order if it has none. After the last entry ended, replay it |
| Enter | Play the selected row of the playlist you're viewing |
| Up, Down, `j`, `k` | Move the selection. Playback does not change |
| `J`, `K` | Move the selected entry down or up |
| Left, Right | Seek backward or forward 10 seconds. A burst of presses becomes one seek |
| Home | Restart the track from the beginning |
| `-`, `_`, `+`, `=` | Volume down or up by 5% |
| `s`, `p` | Stop, play |
| `[`, `]` | Previous or next entry in the playing playlist. Never wraps |
| `d` | Remove the selected entry |
| `a` | Type a path or an `http(s)://` URL to add to the viewed playlist |
| `c` | Clear the viewed playlist after a `y` confirmation |
| `b` | Open the browser |
| Tab, Shift-Tab | Switch which playlist you're viewing |
| `n`, `r`, `D` | New, rename or delete the viewed playlist (`D` after a `y` confirmation) |
| `z` | Shuffle the viewed playlist |
| `?` | Show the key help |
| `m` | Toggle mouse capture |
| Ctrl-L | Redraw the screen and re-place the cover |
| Esc | Close the open overlay or cancel typing |
| `q`, Ctrl-C | Quit |

With the mouse on, a click selects a row of the viewed playlist, a second click plays it, the wheel scrolls it, a click on a tab views that playlist, the transport buttons — `SHFL` for shuffle among them — work, and a click on the progress bar seeks. `tenuto tui --mouse off` leaves the mouse to the terminal, and `--artwork blocks` or `--artwork off` change how the cover is drawn. The [reference](https://github.com/alvytsk/tenuto/blob/main/docs/reference.md#the-terminal-player) covers the options, the layout tiers and every message the player can answer with.

### The browser

`b` opens a browser with three tabs. Files shows one directory at a time. Podcasts shows your subscriptions and their cached episodes. Radio shows your saved stations.

| Key | Action |
|---|---|
| Up, Down, `j`, `k` | Move |
| Tab | Switch between Files, Podcasts and Radio |
| Enter | Open a directory or a feed. Add a file, an episode or a station to the playlist you had open when you pressed `b`. On a row already added, remove it instead |
| Space | Mark several rows to add together. On Files, a folder can be marked too |
| `a` (Files) | Add the marked rows, or the one under the cursor, recursively — files and folders alike |
| Backspace, Left | Go up one level |
| `a` (Podcasts) | Subscribe by URL |
| `a` (Radio) | Add a station by its stream URL |
| `r`, `R` (Podcasts) | Refresh the highlighted feed, or every feed |
| `r` (Radio) | Re-probe the highlighted station |
| `d` (Podcasts) | Unsubscribe after a `y` confirmation |
| `d` (Radio) | Remove the station after a `y` confirmation |
| `b`, Esc | Close the browser |

A row already added shows a green `✓`. Opening the browser never refreshes a feed. `r` and `R` do, and so does `tenuto refresh` from a shell.

**Radio.** Adding a station probes its stream once: the ICY identity it reports — name, genre, bitrate, logo — comes back cached, so the list draws on a cold start without a request. A verified row draws its slug, then genre and bitrate; the name itself is not drawn again, since the slug already stands for it. A station whose probe only got a retryable failure (a `429`, a `503`, a reset connection) is saved anyway, shown by its URL with an unreached marker; `r` tries the probe again. A station's logo, when it has one and it decodes, shows in the cover pane while that station plays.

### Playlists

The queue is now several named playlists, shown as tabs above the list: `Tab`/`Shift-Tab` switch which one you're viewing, `n`/`r`/`D` create, rename and delete one, and `z` shuffles the one you're viewing. Playback stays on whichever playlist is playing while you look at another; Enter in any of them plays from it. Deleting the one you're viewing moves you to the next tab over. Adding appends and never interrupts what is playing. Every playlist is saved and survives a restart. When a track ends the next one, in playback order, starts from its own saved position, and the last one simply ends. Before a track is loaded its row shows what was saved: `12:34 saved`, `~12:34 saved` for an estimate, or `played` for a finished track. Local files and plain URLs now always start at 0:00 on a fresh load — a podcast episode still resumes. The [reference](https://github.com/alvytsk/tenuto/blob/main/docs/reference.md#playlists) has the caps and the exact rules.

## Podcasts from the shell

| Command | Action |
|---|---|
| `subscribe <url> [--as <slug>]` | Fetch a feed once, store the subscription, cache its episodes |
| `feeds` | List every subscription |
| `episodes <slug> [-n N] [--reverse]` | List a feed's cached episodes with your progress |
| `play <slug> <index>` | Play an episode by the number `episodes` shows |
| `refresh [<slug>]` | Refresh one subscription, or all of them |
| `unsubscribe <slug>` | Remove a subscription and its cache. Saved positions are kept |

```text
$ tenuto episodes radio-t
  #  PROGRESS            AUDIO  PUBLISHED (UTC)  TITLE
  1  23:14               -      2026-09-06       Радио-Т 987
  2  ~18:02 / (1:42:00)  -      2026-08-30       Радио-Т 986
  3  played              -      2026-08-23       Радио-Т 985
  4  position unknown    none   2026-08-09       Bonus: outtakes
```

Everything after `subscribe` reads the local cache, so listing and playing work offline. Nothing refreshes on its own: a feed's episode list changes only when you run `refresh`. Only the episode list is cached. Playing an episode streams it every time.

The slug is the short name you use in commands. Pass `--as` to choose it, or let Tenuto derive one from the feed's title. `~` marks an estimated position, `played` a finished episode, and the duration in parentheses is the feed's own claim, unverified. Slug rules, episode numbering, what a refresh reports and what happens when a feed moves its audio are all in the [reference](https://github.com/alvytsk/tenuto/blob/main/docs/reference.md#podcasts).

## Playing over HTTP

- A server that supports range requests can seek and resume. Most podcast hosts do, including for MP3 files with no seek index.
- A server without range support plays through from the start and cannot seek or resume.
- A live stream (Icecast, Shoutcast v2) plays without a position bar. It cannot seek or restart, and is never resumed: pausing closes the connection and playing rejoins the live edge.
- If a live stream drops, Tenuto reconnects with backoff for up to five minutes, then fails; Space tries once more. Stop, pause, or another track cancels it immediately.
- A dropped connection on a finite track fails. Playing again makes one attempt to reopen at the saved position.
- A stream that interleaves ICY metadata, an HLS playlist, or a source whose continuity cannot be established is refused.

```sh
tenuto play https://example.org/stream
```

A seek inside an MP3 with no seek index lands on an estimate, which can be some way off on a long variable-bitrate file. Such positions are shown with a leading `~`. [Seeking accuracy](https://github.com/alvytsk/tenuto/blob/main/docs/reference.md#seeking-accuracy) explains why.

## Where Tenuto keeps things

| Location | Contents |
|---|---|
| `$XDG_STATE_HOME/tenuto/state.json` | Saved positions, volume and your playlists |
| `$XDG_STATE_HOME/tenuto/logs/` | One log per player run |
| `$XDG_DATA_HOME/tenuto/subscriptions.json` | Your subscriptions |
| `$XDG_DATA_HOME/tenuto/stations.json` | Your saved radio stations |
| `$XDG_CACHE_HOME/tenuto/feeds/` | Cached episode lists |

On macOS these resolve to the platform's own directories. The cache and the logs can be deleted at any time. Subscriptions and saved positions cannot be recovered once deleted.

## When something goes wrong

**`Another Tenuto player is using this state profile`.** Only one player runs per profile. Quit the other one. Feed commands can run beside a player.

**`no cached episodes for radio-t; run tenuto refresh radio-t`.** The feed's cache is missing or unreadable. `refresh` rebuilds it. The same command fixes a `corrupt cache` message.

**`not saving` in the player's header.** A write to the state file failed. The player keeps running and tries again on the next save. `unsaved` means the state file could not be repaired at startup and the session runs without saving.

**Something else.** While the player runs, everything it would have printed to standard error goes to a log file under the logs directory above, one per run. `RUST_LOG=tenuto=debug tenuto` writes more. A crash prints its message on the restored terminal.

## Development

```sh
cargo run --locked -- play <path-or-url>
cargo run --locked -- tui
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo publish --dry-run --locked
```

CI runs every gate above on each pull request. The tests run on Linux, and on macOS as a non-blocking leg. Runtime code forbids unsafe code and denies `unwrap` and `expect`. `TENUTO_AUDIO_OUTPUT=null` runs the player against a paced virtual output on a machine with no sound device.

- [Reference](https://github.com/alvytsk/tenuto/blob/main/docs/reference.md): the exact rules for the player, the queue, feeds, files and state, and the known limitations.
- [Architecture](https://github.com/alvytsk/tenuto/blob/main/docs/architecture.md): the C4 views, the execution contexts and the contracts.
- [Changelog](https://github.com/alvytsk/tenuto/blob/main/CHANGELOG.md).

## License

[MIT](https://github.com/alvytsk/tenuto/blob/main/LICENSE) © 2026 Alexey Vymyatnin
