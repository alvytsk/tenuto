# M12 — YouTube podcasts and music, without ffmpeg

Status: design, approved for planning. Live YouTube streams are a later
milestone (M13) and are out of scope here.

## 1. Intent

The user listens to YouTube the way they listen to radio-t: long talk shows
from a few channels, and single music videos mixed into playlists with local
files. Today that means a browser tab. The goal is to paste a YouTube link
into tenuto and have it behave exactly like the things tenuto already does:

- a **YouTube channel is a podcast subscription** — listed in the Podcasts
  tab, refreshed only on `r`/`R`/`tenuto refresh`, episodes newest first,
  each episode resuming where it stopped;
- a **YouTube video is a playlist entry** — added with `a`, played in order
  with local files, starting from 0 on a fresh load like any music.

The reference point is cliamp (`github.com/bjarneo/cliamp`), which shells out
to `yt-dlp | ffmpeg`. tenuto does **not** copy that: it uses yt-dlp only to
resolve a short-lived direct audio URL and plays that URL through its own
HTTP + symphonia pipeline. No ffmpeg.

## 2. What the spike established (2026-10-18)

Run against the user's real links, through the user's network:

- Every non-live video offers audio-only **itag 140**: AAC-LC (`mp4a.40.2`)
  in fragmented MP4 (`ftyp`, `moov`, `sidx`, then `moof`/`mdat` pairs),
  ~129 kbps, 44.1 kHz stereo. Opus/webm (249/250/251) also exists; not used.
- `tenuto play --probe-only <googlevideo URL>` on today's code reports
  `44100 Hz 2 ch, duration 4007.9s, continuity=Finite seek=Native
  resume=Supported`. Decoding from the start works.
- On a local copy of the same file, `DecodedSource::seek_refined` to 10 %,
  50 %, 90 % and end-of-file each lands frame-exact in < 5 ms, with real
  audio after. symphonia's fMP4 handling is good enough.
- **Over HTTP, seeking hung.** googlevideo throttles an open-ended
  `Range: bytes=N-` to ~12 KB/s — below the 16 KB/s the audio needs — while a
  bounded `Range: bytes=N-(N+10MB-1)` downloads at ~9 MB/s. tenuto today
  always sends open-ended ranges (`src/http/service.rs`, `bytes={start}-`).
  This is the one change the existing HTTP path needs.
- The direct URL expires after a few hours and is bound to the IP that
  requested it, so it must never be persisted and must be fetched over the
  same route yt-dlp used.
- Live videos offer **only** muxed video+audio HLS (itags 91–96); that is why
  live is a separate milestone.
- A channel's Atom feed exists at
  `https://www.youtube.com/feeds/videos.xml?channel_id=UC…`, but it carries
  only the latest 15 uploads, and for the user's channel 13 of 15 were
  Shorts. The long-form uploads playlist feed,
  `…/feeds/videos.xml?playlist_id=UULF<channel id without "UC">`, carried 15
  real episodes and no Shorts. Its entries are ordinary Atom: `<id>` is
  `yt:video:<id>`, `<link rel="alternate">` is the watch URL, `<published>`
  is RFC 3339, there is no enclosure.

### The user's network

YouTube is DNS-blocked on the user's machine; a Happ (Xray/Hysteria2) tunnel
exposes HTTP `127.0.0.1:10809` and SOCKS `127.0.0.1:10808`. Through the proxy,
YouTube answers every yt-dlp client (web, mweb, tv, tv_simply, web_safari,
web_embedded, android_vr, ios) with "Sign in to confirm you're not a bot":
the exit IP is flagged. With `--cookies-from-browser firefox:~/.config/zen`
it works. Hence the two settings in §6.

## 3. Goals

1. Subscribe to a YouTube channel from any link to it (a video, `@handle`,
   `/channel/UC…`), via the Podcasts tab's `a` and `tenuto subscribe`.
2. List, refresh and play its episodes exactly like an RSS podcast, with
   resume from the checkpoint.
3. Add a YouTube video to a playlist with `a` and play it like a local file.
4. Seek and resume anywhere in a YouTube track with the same latency as an
   ordinary podcast.
5. A settings screen with a YouTube-only proxy and an optional browser to
   take cookies from.

## 4. Non-goals (this milestone)

- Live streams, in any tab. Adding makes no request, so a live link cannot
  be told apart at add time (and `/live/ID` often points to a finished,
  ordinary recording): it is accepted when added and refused **when played**,
  if yt-dlp reports it is currently live, with a message that says live
  YouTube is not supported yet. *(Builder-check call.)*
- Expanding YouTube playlists: a `&list=…` link adds only its one video.
  Subscribing to a YouTube playlist link (`playlist?list=…`, including
  YouTube's own "Podcasts" playlists) is refused as unsupported.
  *(Builder-check call; the user's podcast link is a video link.)*
- Search, downloads, YouTube/YouTube Music library sign-in (OAuth), likes.
- Episodes older than the feed's latest 15 (no back catalogue via yt-dlp).
- Opus/webm formats, Shorts.
- Installing yt-dlp for the user.
- A "test connection" action in Settings — the first play is the test.

## 5. Anti-goals — failure even if it technically works

- **No ffmpeg**, not even optional.
- **No network request without a user action.** Adding a video to a playlist
  makes no request (the entry shows a placeholder until first played — see
  §7.3). Opening Settings makes no request. Refresh stays explicit.
- **Non-YouTube traffic never goes through the proxy.** radio-t, other feeds
  and stations keep connecting directly.
- No path typing for cookies; no cookies by default.
- Persisting a googlevideo URL anywhere (state, cache, logs unredacted).
- Breaking an existing `state.json` or making older builds refuse it.

## 6. Settings

`,` opens a Settings overlay over the player, like help. One section for now:

```
┌ SETTINGS ──────────────────────────────────────────┐
│  YouTube                                           │
│    Proxy          http://127.0.0.1:10809           │
│  ▌ Cookies from   none                            │
│    yt-dlp         found 2026.08.19                 │
│                                                    │
│  Only YouTube requests use the proxy. Set cookies  │
│  only if YouTube asks you to sign in.              │
└────────────────────────────────────────────────────┘
 Enter Edit   d Clear   Esc Close
```

- **Proxy** — Enter starts typing (same text-entry behaviour as `a`/`n`/`r`),
  Enter saves, Esc cancels, `d` clears. Empty means direct. Accepted:
  `http://host:port` and `https://host:port`. Anything else (including
  `socks5://`) is refused with a notice; adding SOCKS would need reqwest's
  `socks` feature and is not needed for the user's setup. *(Builder's call
  on exact validation; this is the minimum.)*
- **Cookies from** — `none` by default. Enter opens a picker listing
  `none` plus the browsers detected on this machine; choosing one stores the
  yt-dlp `--cookies-from-browser` spec. Detection is by profile directory,
  local only, e.g. Firefox `~/.mozilla/firefox`, Zen `~/.config/zen` or
  `~/.zen` (spec `firefox:<dir>`), LibreWolf `~/.librewolf`
  (`firefox:<dir>`), Chromium `~/.config/chromium`, Chrome
  `~/.config/google-chrome`, Brave `~/.config/BraveSoftware/Brave-Browser`.
  The list shown is display names only.
- **yt-dlp** — read-only: `found <version>` or `not found — install yt-dlp`.
  Computed by running `yt-dlp --version` when the overlay opens (local
  process, no network).

Settings are stored in `$XDG_CONFIG_HOME/tenuto/settings.json` (new file; use
the existing versioned-JSON-file machinery from M9), written atomically on
each change, and read on the next YouTube request — no restart. A missing
file means defaults. A corrupt file is reported and defaults apply; the
first save from the overlay moves it aside (`settings.json.rejected-<ts>`)
and writes a fresh one, exactly as the other stores behave
(`JsonFile::load`). *(Builder-check call.)*

**What the proxy covers:** every yt-dlp invocation (`--proxy`), every HTTP
request tenuto makes to a YouTube host — `www.youtube.com` (feeds),
`*.googlevideo.com` (audio), `i.ytimg.com` (thumbnails) — and nothing else.
yt-dlp and the audio fetch must share the route, because the audio URL is
bound to the requesting IP.

**Bot wall:** when yt-dlp's error says "Sign in to confirm you're not a
bot", the status line reads
`YouTube wants a sign-in: set Cookies from in Settings (,)`.

## 7. Behaviour

### 7.1 Recognising YouTube links

A YouTube video link is any of `youtube.com/watch?v=ID`, `youtu.be/ID`,
`youtube.com/live/ID`, `youtube.com/shorts/ID`, `music.youtube.com/watch?v=ID`,
with or without `www.`/`m.` and tracking params (`si`, `t`, `list`, …).
It is canonicalised to `https://www.youtube.com/watch?v=ID` — that string is
what gets stored. A channel link is `youtube.com/@handle`,
`youtube.com/channel/UC…`, `/c/…`, `/user/…`.

### 7.2 Subscribing to a channel

`a` on the Podcasts tab and `tenuto subscribe <url>` accept a video link or a
channel link. tenuto runs yt-dlp once (through the proxy, with cookies if
set) to learn the channel ID, then subscribes to
`https://www.youtube.com/feeds/videos.xml?playlist_id=UULF<ID without UC>`
as an ordinary Atom subscription: cached, refreshed only explicitly. Feed
fetches for this URL go through the proxy. A live-video link on the Podcasts
tab subscribes to its channel like any other video link.

**Flaky endpoint (user's decision).** The `UULF` feed intermittently answers
404 for a valid channel — 2 of 4 fetches on 2026-10-19, while the plain
`channel_id` feed answered 4 of 4. A `404` on a YouTube feed fetch is
retried quietly a couple of times (short, bounded delay) within the same
refresh or subscribe before it is reported as a failure. This is still one
user-initiated refresh: bounded, cancellable, never scheduled. Other feeds
keep today's single-attempt behaviour.

**Feed title.** The `UULF` feed's `<title>` is the generic `Videos`
(verified 2026-10-19); the channel name is in the feed's `<author><name>`
(`Мы обречены`). For YouTube feeds the subscription's title — and the slug
derived from it unless `--as` is given — comes from `<author><name>`, on
subscribe and on every refresh alike, so refresh never overwrites it with
`Videos`.

Episodes need no new identity: the Atom `<id>` (`yt:video:<id>`) is the
GUID, so checkpoints are `PodcastEpisode { feed, episode }` as today and
resume works unchanged. An entry has no enclosure; its alternate link is a
YouTube watch URL. Such an episode is **playable** (not `AUDIO: none`): its
source is the watch link, resolved at play time (§7.4).

The feed's 15-entry window means an episode that drops out of the feed on a
refresh disappears from the list the way any feed item would; its checkpoint
remains. Recorded as a known limitation.

### 7.3 Adding a video to a playlist

`a` in the player takes a YouTube video link like any URL and appends the
canonical watch URL. No request is made. Until the entry is first played its
row shows `youtube:<ID>`; after a play it shows the title yt-dlp reported,
kept with the entry so it survives a restart.

Identity is `MediaId::RemoteUrl` of the canonical watch URL. **No new
`MediaId` variant, no `state.json` schema bump.** A fresh load starts at 0
(music rule, M8); stop/pause/transport recreation preserve position as for
any media. The same video as a feed episode and as a playlist entry are two
identities, consistent with "A podcast episode and a direct URL are different
things" in `reference.md`.

### 7.4 Playing

Whenever a YouTube watch URL is about to load (episode or entry, CLI `play`
or TUI):

1. Run `yt-dlp --no-playlist -f 140 --print <fields> [--proxy P]
   [--cookies-from-browser B] -- <watch URL>` and read the direct audio URL,
   title, duration, and thumbnail. If 140 is absent, fall back to
   `bestaudio[ext=m4a]`; if that is absent too, fail with "no AAC audio for
   this video". A live video is refused here with the "not supported yet"
   message.
2. Hand the direct URL to the existing HTTP prepare path with the title as
   fallback title.
3. Fetch googlevideo with **bounded ranges** (10 MB chunks, continuing
   seamlessly), never open-ended.
4. On HTTP 403/410 from googlevideo mid-play or on resume after a long pause
   (the URL expired), re-run step 1 once and continue at the same position.
   This sits under the existing finite-media recovery (§7.6 of
   `architecture.md`): bounded, cancellable, never across a restart.
5. Cover art: the video thumbnail as a JPEG (`i.ytimg.com/vi/<ID>/hqdefault.jpg`
   is fine — the `image` crate here has jpeg/png only), through the proxy;
   shown in the player and published to MPRIS like any cover. *(Builder's
   call on which thumbnail URL.)*

`tenuto play <youtube link>` works too, by the same path.

### 7.5 Where resolution runs

`PlayerRuntime::locate` runs on the main thread and must stay a local
lookup; a yt-dlp run takes seconds. Every open of a source, by contrast,
already goes through one function on the decode thread,
`playback::prepare::prepare(location, context)`: a load, a station's
`fresh_open`, a finite recovery (`reconnect_finite`) and the reopen after
Stop or a recovery pause (`ensure_source_open`). Resolution goes there.

- `SourceLocation` gains a third variant carrying the canonical watch URL.
  `locate` returns it for a YouTube entry or episode without running
  anything. The engine's `descriptor` therefore holds the watch URL, never
  a googlevideo URL, so nothing that persists or logs a descriptor can leak
  one.
- `prepare` resolves that variant through a resolver handed in on
  `PrepareContext`, the way the HTTP service and the clock are injected
  today, then continues exactly as `open_http` with the direct URL. The
  resolver is concrete, not a trait: it runs a configured yt-dlp
  executable, which tests point at a stub (§8). It lives in a sources-rank
  module, so `playback` may import it and `tests/m9_layering.rs` is not
  loosened.
- Because every reopen passes through `prepare`, Stop → Play, a finite
  recovery and Play after a long pause resolve again with no new code
  path. To keep a plain Stop → Play fast, the worker keeps the last direct
  URL for its descriptor, in memory only, and reuses it. A 403 or 410 from
  googlevideo, or a failed open of the kept URL, discards it and resolves
  once more within the same open (§7.4 step 4). A new `Load` or shutdown
  drops it.
- The yt-dlp child is cancellable: a stop, a replacing load or a shutdown
  on the interrupt word kills it, and the open reports `Cancelled`, as a
  cancelled HTTP open does today.
- The resolved title and thumbnail URL reach the front ends on the
  `Loaded` event's `MediaMetadata`: the title as a fallback below any tag,
  the thumbnail as the remote cover source the runtime's `Covers` fetches,
  once, for both the player and the desktop widget.

### 7.6 Errors (status line / CLI stderr)

- yt-dlp not on `PATH`: `yt-dlp is not installed`.
- Bot wall: as in §6.
- Any other yt-dlp failure: its first error line, URLs redacted.
- Proxy unreachable: the existing network error wording.

All yt-dlp stderr and every direct URL pass through `redact_url` before any
log or message; cookie specs are fine to log, cookie values never appear.

## 8. Constraints

- Layering rules in `CLAUDE.md` and `architecture.md` §4–5 hold:
  `playback` never imports persistence or blocks on Tokio; `library` never
  prints; `tui` changes state only through `Session`; `tests/m9_layering.rs`
  is not loosened. The yt-dlp call is a blocking subprocess and must run
  where blocking is allowed (the load path that already does blocking
  network work), never on the TUI thread or a Tokio worker.
- Runtime code: no `unsafe`/`unwrap`/`expect`.
- yt-dlp is invoked with `--` before the URL, always.
- CI has no YouTube access: yt-dlp is exercised through a stub executable
  (like `scripts/release/test.sh` stubs `gh`/`curl`) and googlevideo through
  the existing local fake HTTP servers. No test touches the real network.
- Engine/runtime tests follow the virtual-clock rules in `CLAUDE.md`.
- Linux is the verified platform; nothing macOS-specific is required.

## 9. Docs and acceptance

- `reference.md`: Settings overlay and `,` key; YouTube links in `a`,
  `subscribe`, `play`; the 15-episode limit; settings file in "Files and
  state"; yt-dlp requirement.
- `architecture.md`: where yt-dlp resolution sits, bounded-range fetching,
  the proxy's scope.
- `CHANGELOG.md` `[Unreleased]`.
- `docs/m12-acceptance.md`, with a manual pass in Ghostty on the user's
  machine: subscribe to «Мы обречены» from the podcast link, play an
  episode, seek to the middle, quit, reopen, resume; add the music link to a
  playlist, play, check title and cover appear; clear cookies and see the
  sign-in message.

## 10. Calls made in this document (not the user's words)

- Identity reuses `PodcastEpisode` (GUID = Atom id) and `RemoteUrl`
  (canonical watch URL) instead of the new `MediaId` variant floated in
  conversation — same behaviour, no schema bump.
- Long-form `UULF` playlist feed rather than the channel feed, to drop
  Shorts.
- itag 140 with `bestaudio[ext=m4a]` fallback; 10 MB range chunks.
- HTTP(S) proxies only; settings as JSON in `$XDG_CONFIG_HOME/tenuto/`.
- Thumbnail as cover art via the proxy.
- Re-resolve once on 403/410.
- Resolution in `prepare` on the decode thread, behind a third
  `SourceLocation` variant, with the direct URL kept in memory per
  descriptor (§7.5).
- Live links accepted on add, refused on play; YouTube playlist links not
  subscribable; corrupt `settings.json` handled like the other stores;
  YouTube feed title from `<author><name>`.
