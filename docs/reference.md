# Tenuto reference

The rules the player and the feed commands follow, stated precisely. The [README](../README.md) is the place to start; this page is where it points when a detail matters.

## Command line

| Command | Action |
|---|---|
| _(no arguments)_ | Open the full-screen player on the saved playlists |
| `play <path-or-url>` | Play one file or URL with a status line and a few keys |
| `play <slug> <index>` | Play a subscribed feed's episode by its 1-based index |
| `play ... --probe-only` | Open the source, print what was found, and exit without a device or a terminal |
| `tui [--mouse on\|off] [--artwork auto\|blocks\|off]` | Open the full-screen player on the saved playlists |
| `subscribe <url> [--as <slug>]` | Fetch a feed once, store the subscription, cache its episodes |
| `feeds` | List every subscription |
| `episodes <slug> [-n N] [--reverse]` | List a feed's cached episodes with your progress |
| `refresh [<slug>]` | Refresh one subscription, or all of them |
| `unsubscribe <slug>` | Remove a subscription and its cache. Checkpoints are kept |

`play` uses these keys: space pauses or resumes, the arrow keys seek, `s` stops, `p` plays, and `q` quits.

## Playing over HTTP

- A range-capable server can seek and resume. This includes MP3 files with no seek index, which is most podcasts.
- A range-less server plays through from the start. It cannot seek or resume.
- A live stream (Icecast, Shoutcast v2) plays without a position bar. It cannot seek or restart, and is never resumed: pausing closes the connection and playing rejoins the live edge.
- If a live stream drops, Tenuto reconnects with backoff for up to five minutes, then fails; Space tries once more. Stop, pause, or another track cancels it immediately.
- A dropped connection on a finite track fails. Playing again makes one attempt to reopen at the saved position.
- A stream that interleaves ICY metadata, an HLS playlist, or a source whose continuity cannot be established is refused.

### Seeking accuracy

A seek on MP3 computes a byte offset instead of scanning forward, which keeps a seek on a long podcast fast. A file with a Xing, Info or VBRI header lands exactly, or within a fraction of a second for variable-bitrate audio.

A file with no such header lands on a rough estimate. The landing can be in a substantially different part of the recording. On a worst-case 600-second variable-bitrate file, a seek to one third of the way through landed five seconds from the end. Constant-bitrate files without a header land exactly, but nothing in the file says which kind it is before the seek runs. Every landing on an index-less MP3 is therefore reported as an estimate, never as a confirmed position. Listings and the player show an estimate with a leading `~`.

## The terminal player

```sh
tenuto tui [--mouse on|off] [--artwork auto|blocks|off]
```

`tui` restores every playlist, the playing one's active entry, the volume and every checkpoint. It never starts playing on its own. No track is loaded and nothing is fetched until you press a playback key. Local files' tags and the active local entry's cover are read in the background. The audio device is created on the first load, so the player opens on a machine with no output device.

Options:

- `--mouse off` starts with mouse capture disabled and leaves the terminal's own selection and scrolling alone. `m` toggles it at any time. The default is `on`.
- `--artwork auto` asks the terminal which image protocol it supports and falls back to colored half-blocks after 250 ms without an answer. `blocks` always uses half-blocks. `off` never loads artwork and shows only the placeholder.
- Under tmux, `auto` and `blocks` run `tmux set -p allow-passthrough on` for the current pane. `off` avoids that.

The layout adapts to the terminal size. At 80 columns by 28 rows and above the player shows the cover with the track information, the spectrum and the time beside it, then progress and transport, above the viewed playlist. The spectrum takes the rows the information leaves, so it is taller when a track has only a title. Below 80 columns or 28 rows it is compact: a smaller cover beside the title, the artist, a spectrum of three or four rows and the time. Below 22 rows the compact player shrinks to a small cover and a one-row spectrum. Below 50 columns or 18 rows it is minimal, with no cover and no spectrum. Below 30 columns or 8 rows it asks for a larger window, while space and `q` keep working. Either dimension alone drops a tier.

### Keys

| Key | Action |
|---|---|
| Space | Pause or resume. Before anything is loaded (or after a failed load), resume the *playing* playlist's remembered entry, or its first entry in playback order if it has none — never a row you have selected elsewhere. After the last entry ended, replay it |
| Enter | Play the selected row of the *viewed* playlist, whichever playlist that is |
| Up, Down, `j`, `k` | Move the selection. Playback does not change |
| `J`, `K` | Move the selected entry down or up |
| Left, Right | Seek backward or forward 10 seconds. A burst of presses becomes one seek |
| Home | Restart the track from the beginning |
| `-`, `_`, `+`, `=` | Volume down or up by 5% |
| `s`, `p` | Stop, play |
| `[`, `]` | Previous or next entry, in the playing playlist's playback order (or, while a track is loading, the playlist that request came from). Never wraps; with no current track they do nothing |
| `d` | Remove the selected entry |
| `a` | Type a path or an `http(s)://` URL to add to the viewed playlist |
| `c` | Clear the viewed playlist after a `y` confirmation |
| `b` | Open the browser |
| Tab, Shift-Tab | View the next or previous playlist |
| `n` | Create a playlist |
| `r` | Rename the viewed playlist |
| `D` | Delete the viewed playlist after a `y` confirmation; refused outright if it is the only one |
| `z` | Toggle shuffle on the viewed playlist |
| `?` | Show the key help |
| `m` | Toggle mouse capture |
| Ctrl-L | Redraw the screen and re-place the cover |
| Esc | Close the open overlay or cancel typing |
| `q`, Ctrl-C | Quit |

Ctrl-C quits and Ctrl-L redraws from anywhere, including while typing and inside overlays. Every other key belongs to what is open. While typing after `a`, `n` or `r`, printable keys are text, Enter submits and Esc cancels. The clear and delete confirmations take `y` and treat any other key as no. A Ctrl or Alt chord never fires a plain shortcut.

Seeking before anything is loaded answers `Play a track before seeking`. While a track is loading it answers `Still loading`. After the last entry ended, Left and Right answer `Track ended; press play to replay`. An empty *playing* playlist answers `Queue is empty` on Space, `p`, Home and seeking — unless a track is already Playing, Paused or Reconnecting, which keep taking those commands even after their playlist is emptied out from under them; only a Stopped track falls back to the notice. Enter always follows the *viewed* playlist and answers `Queue is empty` only when that one has nothing to select, whatever state the playing playlist is in.

With mouse capture on, a click selects a row of the viewed playlist and a second click on the selected row plays it. The wheel moves the selection over that playlist. A click on a tab of the strip views that playlist; the one-row strip of the smallest layout is keyboard-only. The transport buttons act like their keys, and the `SHFL` button after them toggles shuffle on the viewed playlist as `z` does, lit while it is on. A playlist or a browser listing longer than its pane shows a scrollbar thumb on its right border; it is display only. A click on the progress bar seeks, only for a loaded track whose duration the decoder confirmed. The mouse does nothing while an overlay is open.

### Playlists

The queue is now one of several named playlists, shown as tabs above the list. At most 32 playlists exist at once, each named 1 to 40 characters after trimming — names may repeat, since a playlist's identity is its ID, not its name; deleting the last one is refused. Together they hold at most 4,096 entries, not 4,096 each.

Two playlists matter independently: the one you are *viewing* (what the list shows, what `Enter`, `a`, `c`, `r`, `D` and `z` act on) and the one that is *playing* (what Space, `p`, `[` and `]` act on, and what a track ending advances). They are usually the same tab, but switching tabs with `Tab`/`Shift-Tab` only changes which one you are viewing — playback keeps running on whichever playlist it was already on. Enter on another tab's row starts playing that playlist, which makes it both viewed and playing at once. The view starts on the playing playlist each time the player opens; which tab you had open is not remembered across a restart.

Deleting the viewed playlist with `D` moves the view to the adjacent one — the tab that took its place in the strip, or the one before it if the deleted tab was last. Deleting the *playing* playlist follows the same rule for `playing`: whatever it had adopted is released and playback stops, and the `▶` mark moves to that same adjacent playlist.

On the tab strip, the playing playlist's name is prefixed with `▶`; the viewed tab is bold; a shuffled playlist's name is suffixed with `·shfl`. An existing queue from before this feature becomes a playlist named `Default`.

`z` toggles shuffle on the viewed playlist. The list still displays in its own order; only playback order — next, previous and what plays next when a track ends — follows the shuffle. Turning it off resumes in list order from wherever playback is. The shuffled order survives a restart. Adding to a playlist while its shuffle is on reshuffles it with the current track first, so everything added lies ahead; tracks already played on this pass come round again. Removing entries leaves the order of the rest alone.

### The browser

`b` opens a browser with three tabs. Files shows one directory at a time, starting at the active local entry's directory or the directory `tui` started in. Podcasts shows the cached subscriptions. Radio shows the saved stations. `Tab` cycles Files → Podcasts → Radio → Files.

| Key | Action |
|---|---|
| Up, Down, `j`, `k` | Move |
| Tab | Switch between Files, Podcasts and Radio |
| Enter | Open a directory or a feed. Add a file, an episode or a station to the playlist the browser opened on. On Files with rows marked, add all of them the way `a` does. On a row already queued, remove it instead |
| Space | Mark several rows to add together. On Files a directory can be marked too. Rows already queued are skipped |
| `a` (Files) | Add the marked rows, or the row under the cursor, to the playlist the browser opened on — files and folders alike, recursively, in this listing's own order; strictly additive |
| Backspace, Left | Go up one level |
| `a` (Podcasts) | Subscribe by URL |
| `r`, `R` (Podcasts) | Refresh the highlighted feed, or every feed |
| `d` (Podcasts) | Unsubscribe after a `y` confirmation |
| `a` (Radio) | Add a station by its stream URL |
| `r` (Radio) | Re-probe the station under the cursor |
| `d` (Radio) | Remove the station under the cursor, after a `y` confirmation |
| `b`, Esc | Close the browser |

A row already in a playlist shows a green `✓`. So does a directory on Files when that playlist holds a file from anywhere inside it — any file, not every file, and not one reached through a symlink; `a` on such a directory still adds whatever is missing. A feed's episodes are listed newest first, with undated ones after the dated ones in feed order. `tenuto episodes` keeps feed order, so its indices do not move. A directory listing is read one level at a time; `a` on Files is the exception, walking a folder's whole tree to add it. Whichever playlist was viewed when the browser opened is where every add in that session lands, even if you switch playlists — or the browser is still open when a folder walk finishes — before it does.

A folder add reports what happened in the status line, made of the parts that apply, in order: `added N`, `N already queued`, `N unreadable`, `N did not fit`, and `scan limit reached` (which never carries a count, since an unfinished walk cannot know how many files it never reached). Nothing at all to add reports `nothing to add`; a playlist deleted before the walk finished reports `Playlist was deleted; nothing added`. The walk follows the same one-level-at-a-time order the listing itself uses at every depth — subdirectories first, then that level's own audio files, each by case-insensitive name — so a truncated walk keeps what the listing would show first. A directory reached through a symlink is never walked, including a symlink picked directly as a root; a file reached through a symlink is added once, and the same file reached by two different paths (an alias) counts and adds only once.

Opening the browser never refreshes a feed. The Podcasts tab lists what was last cached. Updating it is an explicit act: `r` or `R` in the browser, or `tenuto refresh` from a shell. Enqueueing or restoring a URL or an episode makes no network request. Only playing it does. The same holds for Radio: opening the tab and listing saved stations makes no request; only `a`, `r` and playing a station do.

### The Radio tab

A saved station is added with `a`: type the stream URL and press Enter. Tenuto opens it once, through the same path playback itself uses, and keeps whatever ICY identity comes back — name, genre, bitrate, logo — none of which is guaranteed present. A row with an identity draws its slug, then `genre · bitrate kbps`, with genre and bitrate left out when absent — the name is not drawn again, since the slug already stands for it. A URL that answered with a retryable failure (`429`, `503`, a reset connection) is saved as an unverified candidate: the row shows its URL and an `(unreached)` marker instead, and `r` tries the probe again. A URL that is positively not a station — a finite file, an HLS playlist, an `icy-metaint` response, or a non-retryable failure such as `404` — is never saved at all, and the browser shows an error notice.

`d` asks `Remove <slug>? y/N` before dropping a station; any key other than `y` cancels. Adding a URL that is already saved re-probes the existing station instead of creating a duplicate. Enter enqueues a station exactly as Enter enqueues a file or an episode, and Enter again on a queued row removes it — the Radio tab introduces no transport verb of its own.

A station's logo, when its identity carries one and it decodes, shows in the player's cover pane while that station plays; it is fetched only once playback has opened a network connection, never while the tab merely lists or the station sits enqueued or restored. The logo refreshes only on add or re-probe, never mid-playback, so a station that changes its logo shows the old one until re-probed.

`stations.json` is written atomically, exactly as `subscriptions.json` is. A file this build cannot parse is quarantined to `stations.json.rejected-<timestamp>` and the station list starts empty rather than being silently truncated; a file from a newer schema version is left in place with station writes disabled for the session.

### Playlist entries

Adding appends to the playlist you added to and never changes what is playing. Every playlist is saved in `state.json` with the checkpoints and survives a restart. The same track may appear twice, in the same playlist or different ones. Duplicates share one listening history but keep their own places. When a track ends, the next entry in its playlist's playback order starts from its own resume point. A finished entry replays from the beginning. The playlist's last entry in that order simply ends — there is no wrap or repeat. A load that fails leaves every playlist alone and waits for you.

All playlists together hold at most 4,096 entries. A single add that would go past that is refused whole with `Playlists are full (4096 entries in total)`. A folder add is different: it takes however many still fit and reports the rest as `did not fit` rather than refusing the whole batch. Checkpoints keep their own, separate cap of 512 media, unrelated to the entry cap. A queued track's history can still be evicted by enough other listening. The entry stays queued and then starts from zero.

An entry for a podcast episode remembers the episode, not a list position. Before loading it, the player looks the episode up in the local feed cache and uses its current enclosure. When the episode, the subscription or the cache is gone, it plays the URL it last saw and says `Using saved episode source`.

**Resuming.** A podcast episode resumes from its checkpoint on every fresh load, exactly as before M8. A local file or a plain URL — including from `tenuto play` — always starts from the beginning on a fresh load, even with a checkpoint on record; only pausing and pressing Stop then Play still continue mid-track, and a checkpoint is still written for it either way. Reaching the end of a track marks it complete regardless of kind; reopening a completed track starts from the beginning.

Before the active entry is loaded, the progress line and its playlist's rows show saved history, not a live position:

| Label | Meaning |
|---|---|
| `12:34 saved` | The checkpoint's resume point |
| `~12:34 saved` | An estimated resume point |
| `played` | Finished. Playing it starts from the beginning |
| `position unknown` | A checkpoint without a position |

### Cover art

A podcast episode's cover is the feed's `itunes:image`, the episode's own first, then the channel's, as recorded at the last refresh. It is downloaded only after playback has opened a network connection. A podcast with no feed image, and a plain URL entry, show the front cover embedded in the stream's own tag once the track is loaded. A local entry uses its embedded cover, then `cover.jpg`, `cover.png`, `folder.jpg` or `folder.png` beside it. Until then, and when there is none, the placeholder shows.

### Saving, quitting and signals

`q` and Ctrl-C exit 0. SIGINT, SIGHUP and SIGTERM, including a closing pane or window, capture and flush the final position, restore the terminal, and exit with `128 + signal number`. `tenuto play` follows the same contract. A flush that fails is reported as `State was not saved: ...` after the terminal is restored. SIGKILL, a crash or power loss keep only the last completed write.

A write that fails while the player runs shows `not saving` in the header until a later write succeeds. If the state file cannot be repaired safely at startup, the session runs unsaved and shows `unsaved`.

### Logs

While `tui` runs, everything written to standard error goes to a new log file instead of the screen:

```text
$XDG_STATE_HOME/tenuto/logs/tenuto-tui-<UTC timestamp>-<pid>.log
```

Each run creates its own file and keeps the five most recent earlier ones. `RUST_LOG=tenuto=debug tenuto tui` works as usual and lands in that file. A crash prints its panic message on the restored terminal.

## Podcasts

```sh
tenuto subscribe http://feeds.rucast.net/radio-t --as radio-t
tenuto feeds
tenuto episodes radio-t -n 5
tenuto episodes web-standarts --reverse -n 5
tenuto play radio-t 3
tenuto refresh radio-t
tenuto refresh
tenuto unsubscribe radio-t
```

`subscribe` fetches the feed once, stores the subscription, and caches the episodes. Everything after that reads the cache. `feeds`, `episodes` and `play` never touch the network for feed data, so they work offline. Nothing refreshes on its own. A feed's episode list changes only when you run `refresh`. There is no background poller, no refresh on listing, and no retry loop.

There is no offline audio. Only the episode list is cached. Playing an episode streams its enclosure over HTTP every time.

### Listing

```text
$ tenuto feeds
SLUG        EPISODES  REFRESHED (UTC)   TITLE
radio-t            4  2026-09-11 18:33  Радио-Т

$ tenuto episodes radio-t
  #  PROGRESS            AUDIO  PUBLISHED (UTC)  TITLE
  1  23:14               -      2026-09-06       Радио-Т 987
  2  ~18:02 / (1:42:00)  -      2026-08-30       Радио-Т 986
  3  played              -      2026-08-23       Радио-Т 985
  4  position unknown    none   2026-08-09       Bonus: outtakes
```

Every timestamp is UTC. No local conversion is attempted.

`PROGRESS` has five states:

| Cell | Meaning |
|---|---|
| `—` | No checkpoint. This episode has never been opened |
| `23:14` | A decoder-confirmed resume point |
| `~18:02` | An estimated position left by a byte-offset seek on an index-less MP3 |
| `played` | Finished. Reopening starts from the beginning |
| `position unknown` | A checkpoint exists but carries no position |

`/ (1:42:00)` beside a position is the feed's own `itunes:duration` claim. It is in parentheses because nothing has verified it. `AUDIO: none` means the item has an identity but no usable enclosure, so there is nothing to play.

A subscription whose cache file has gone shows `—` episodes and `never`, and `feeds` exits zero for it. A cache that exists but cannot be read is an error. See [When the cache is unusable](#when-the-cache-is-unusable).

A state file that cannot be read fails the listing instead of printing every episode as unplayed.

### Indices

Episode indices are 1-based and follow feed order, never a sort by date or title. `-n 5` changes how many rows are displayed, never what an index means. `--reverse` starts from the end of the feed, and `-n` then counts from the end. Every row keeps its index, so `play` resolves the number you read. Indices renumber only when a `refresh` replaces the cache.

Feeds are not all ordered the same way. Radio-T lists its newest episode first. Some feeds list their oldest first, so `--reverse -n 5` shows their five newest.

### Slugs

`--as <slug>` names a subscription explicitly. A slug is 1 to 32 ASCII lowercase letters, digits or hyphens. An explicit slug that is taken is refused.

Without `--as`, the slug comes from the feed's title. ASCII letters are lowercased and kept, digits are kept, and every other run of characters collapses to one `-`. No transliteration is attempted. A title in a non-ASCII script yields nothing, and the slug falls back to the feed URL's host with a leading `www.` stripped. `Радио-Т` at `https://radio-t.com/rss/` becomes `radio-t-com`. A derived slug that collides takes the first free `-2`, `-3` suffix within 32 characters.

### Refreshing and exit status

`refresh <slug>` updates one subscription. `refresh` with no slug updates every one in order, with no concurrency and no retries. Both send a conditional request when a usable cache exists, so an unchanged feed costs a 304.

```text
$ tenuto refresh
radio-t: updated, 412 episodes retained, 3 skipped
sysdesign: failed: network error while Open: ...
tenuto: 1 of 2 feeds did not complete successfully
```

A batch prints every feed, then exits nonzero if any of them did not complete. The rule is general: a partial success never exits zero. `subscribe`, `unsubscribe` and `refresh` each commit in two steps across two files. When the second step fails, the command says exactly what did and did not happen and exits nonzero. Read the line before trusting the status.

### When the cache is unusable

```text
tenuto: no cached episodes for radio-t; run tenuto refresh radio-t
tenuto: corrupt cache for radio-t: cache file is malformed (syntax error at line 1, column 2); run tenuto refresh radio-t
tenuto: cache parser 99 differs from 1 for radio-t; run tenuto refresh radio-t
```

All three name the same recovery. The cache is refetchable data, and `refresh` rebuilds it unconditionally when it is missing, corrupt, or stamped by a parser this build does not recognize. A corrupt file is left where it is. Listing a feed never rewrites, quarantines or deletes anything.

### Identities

A podcast episode and a direct URL are different things to Tenuto, even when the bytes are identical. `tenuto play radio-t 3` checkpoints the episode: the feed's identity plus the item's GUID, or its enclosure URL, or its link, in that order. `tenuto play https://cdn.example.org/987.mp3` checkpoints the URL. Progress does not carry from one to the other.

An item with a GUID keeps its position when the show moves its audio to another CDN. An item with no GUID takes its identity from the enclosure URL, so a move loses its position.

`unsubscribe` removes the subscription and its cache and keeps every checkpoint. Resubscribing mints a new feed identity, so those checkpoints are orphaned and the episodes show as unplayed again. Reattaching would require trusting a feed URL to mean the same feed forever.

### Feed-format limits

- RSS 2.0 and Atom 1.0 only. RSS 1.0 is refused by name. JSON Feed is not supported.
- Bytes decide the encoding. A document that will not decode is refused, not repaired.
- Item titles are display text. An Atom `title[type=html]` is decoded once and then kept literally, markup and all. HTML entities that are not XML entities survive as text.
- A publication date is read as RFC 2822, plus the `UTC` zone spelling real feeds use. A date that will not parse leaves `PUBLISHED` as `—` and keeps the episode.
- An item with no GUID, no enclosure and no link has no identity and is skipped and counted.
- An item with an identity but no usable enclosure is kept and listed with `AUDIO: none`.

## Files and state

| Location | Contents |
|---|---|
| `$XDG_STATE_HOME/tenuto/state.json` | Checkpoints, volume, every playlist and which one is playing |
| `$XDG_STATE_HOME/tenuto/state.lock` | The player lock. Empty, never deleted |
| `$XDG_STATE_HOME/tenuto/logs/` | One log per `tui` run. The five most recent earlier ones are kept |
| `$XDG_DATA_HOME/tenuto/subscriptions.json` | Subscriptions. Durable user data |
| `$XDG_DATA_HOME/tenuto/subscriptions.lock` | The subscription writer lock |
| `$XDG_DATA_HOME/tenuto/stations.json` | Saved radio stations. Durable user data |
| `$XDG_CACHE_HOME/tenuto/feeds/<feed-id>.json` | Cached episodes. Refetchable |

On macOS these resolve to the platform's own data, cache and local-data directories. The cache and the logs are disposable. Subscriptions and checkpoints are not.

### Playback state

`state.json` holds one checkpoint per media identity, capped at 512 entries, plus every playlist, capped together at 4,096 entries. It is replaced atomically, so a crash mid-write cannot leave a truncated file. Reaching the end of a track marks it complete. Reopening a completed track starts from the beginning.

`state.json` is schema 4. A schema 3 file (one queue, one active entry) migrates on first load into a single playlist named `Default`. A file this build cannot read is preserved. Garbage is moved aside as `state.json.rejected-<timestamp>`. A file from a build newer than this one — including a schema-4 file read by an older build — is left where it is, with writing disabled for that session, so the player runs but does not save. If only the playlist data is damaged, only that part is reset. The original bytes are copied to `state.json.queue-recovery-<timestamp>` first, and the player says what was reset.

Deleting `state.json` forgets every remembered position. There is no supported way to edit it by hand. A checkpoint key this build cannot parse makes the whole file unreadable.

### One player per profile

`tenuto tui` and `tenuto play` take an exclusive lock on `state.lock` before they read any playback state. A second player on the same profile refuses to start before it opens an audio device or touches the terminal:

```text
tenuto: Another Tenuto player is using this state profile
```

Feed commands and `play --probe-only` take no player lock and can run beside a player. Every subscription change, from the CLI or from the player's browser, holds `subscriptions.lock` for its whole duration. A second one refuses at once with `Another subscription update is in progress`.

## Known limitations

- Non-UTF-8 local paths are unsupported.
- The position is an estimate when device latency is unavailable.
- Seek support over HTTP may stay unknown until the source is probed.
- Linux is the verified platform. macOS builds and runs the suite in CI as a non-blocking leg, with one playback test still failing there. Windows is neither built nor claimed.
