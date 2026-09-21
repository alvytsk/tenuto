# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Playlists. The queue is now one of several named playlists, shown as tabs
  above the list: `Tab` and `Shift-Tab` switch the view, `n` creates one,
  `r` renames and `D` deletes it after a `y`. Playback stays on the playing
  playlist while you look at another; Enter in any playlist plays from it.
  An existing queue becomes a playlist named `Default`.
- Shuffle, per playlist, with `z`. The list keeps its order on screen;
  next, previous and end-of-track follow one shuffled order that survives a
  restart. Turning it off continues in list order from the current track.
- Adding a folder from the Files tab: Space now marks directories too, and
  `a` adds the marked rows — or the row under the cursor — recursively, in
  the browser's own listing order. Directory symlinks are skipped, even
  when selected directly as a root.
- Rows read `Artist – Title`, with the album below.
- Live HTTP radio. An Icecast or Shoutcast v2 stream plays without a
  position bar, from `tenuto play <url>` or the queue. It cannot seek or
  restart and is never resumed: pausing closes the connection and playing
  rejoins the live edge.
- A live stream that drops reconnects with backoff for up to five minutes,
  then fails; Space tries once more. Stop, pause or another track cancels
  the reconnect immediately.
- A Radio tab in the browser over the saved stations: `a` adds a station by
  its stream URL, `r` re-probes it, `d` removes it after a `y`
  confirmation, Enter enqueues it. A row shows the station's slug, genre
  and bitrate from the ICY identity cached when it was added, so the tab
  lists without a network request.
- Stations are saved in `$XDG_DATA_HOME/tenuto/stations.json`.
- A station's logo shows in the cover pane while that station plays.
- SVG cover art, rasterized within a fixed size with every external
  reference refused.
- Seek buttons in the transport row: `◀◀` and `▶▶` step ten seconds, as
  ← and → do.
- Peak caps over the spectrum bars: each holds where its bar last reached,
  then falls more slowly than the bar.

### Changed

- Local files and plain URLs start from the beginning each time they are
  loaded, from `tenuto play` as well as the player. Podcast episodes still
  resume, and pausing or stopping a track still continues where it was.
- The entry limit is 4,096 across all playlists, replacing 256 per queue.
- On a playlist that has never played, `p` and Space start its first track
  in playback order rather than the highlighted row, and next/previous wait
  for a current track. Enter on the highlighted row is unchanged.
- `state.json` is schema 4. A schema 3 file is migrated on first load; an
  older build leaves a schema 4 file in place and runs without saving.
- A seek on a source that cannot seek no longer drops its connection.
- Loading another track interrupts a stalled one immediately.
- A live stream is no longer refused. A stream that interleaves ICY
  metadata and an HLS playlist still are, and the message now names HLS.
- Beside the cover, the title, artist and album now run the full width
  and the spectrum sits under them, in the rows they leave: taller for a
  track with a title alone. A long title is no longer cut at a third of the
  width, and the spectrum is no longer a narrow strip near 80 columns.
- The compact layout is roomier: a larger cover beside the title, the
  artist, a spectrum of three or four rows and the time, with the progress bar on its
  own row and the volume slider where the width allows. A terminal under 22
  rows keeps the previous small player.
- The transport controls are filled buttons with solid glyphs, and the row
  holds still when play turns to pause.

### Fixed

- An MP3 with both an ID3v2 tag and an ID3v1 trailer showed the trailer's
  30-byte title and no cover. The ID3v2 title and cover now win.
- The spectrum stopped short of the right edge at most window widths: the
  columns its bars did not divide were left empty there, up to a third of
  the row. The row now holds as many equal bars as fit, the extra ones
  interpolated between neighbouring bands, and ends at the right edge in
  line with the progress bar. The numbers under the bars count bars.

## [0.1.2] - 2026-09-17

### Fixed

- Opening a slow remote source could report "the server went quiet" as a
  stall when it was the opening deadline that had run out: a wait whose
  budget had been clipped to the remaining opening time reported its own
  phase on expiry. Such a timeout now reports the opening phase.

### Internal

- Three playback tests no longer depend on how fast the machine runs them:
  the retry against a truncated server drives its virtual clock until the
  engine settles rather than for a fixed span, the opening-deadline test
  makes the deadline expire mid-read every time, and the seek-servicing
  test no longer lets its throttled server starve the open.
- CI runs every test binary even after one fails, so a red job shows all
  of its failures rather than the first binary's only.

## [0.1.1] - 2026-09-17

Not published to crates.io; its fix ships in 0.1.2.

### Fixed

- A seek submitted right as the engine was free to act on it could be
  reported as cancelled instead of running: the submitter's wake-up of a
  blocked network read could land on the fetch the seek itself had just
  opened. The wake-up now targets only the read that was in flight when the
  seek was submitted.

## [0.1.0] - 2026-09-17

First release. Published to crates.io as `tenuto`.

### Added

- Plays MP3, FLAC, WAV and M4A files, direct `http(s)://` URLs, and episodes
  of subscribed RSS or Atom feeds.
- Remembers the position in every track, URL and episode, and resumes there.
- Seeks over HTTP with range requests, including MP3 podcasts with no seek
  index.
- A full-screen terminal player with a persistent queue, a file and podcast
  browser, cover art and a spectrum display.
- Feed management from the player: subscribe, refresh and unsubscribe.
- A bare `tenuto` opens the player.

[Unreleased]: https://github.com/alvytsk/tenuto/compare/855fcb5...HEAD
[0.1.2]: https://github.com/alvytsk/tenuto/compare/29da62c...855fcb5
[0.1.1]: https://github.com/alvytsk/tenuto/compare/2167690b3e89979eb61b05b3b3b9af6f69057eb2...29da62c
[0.1.0]: https://github.com/alvytsk/tenuto/commit/2167690b3e89979eb61b05b3b3b9af6f69057eb2
