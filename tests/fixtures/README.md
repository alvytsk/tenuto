# Test fixtures

Self-generated, no third-party content, no licensing constraints.

These are the **audio** fixtures. The RSS and Atom feed documents live in
[`feeds/`](feeds/README.md), which has its own README covering every file
there, the byte construction of the encoding variants that are generated
rather than committed, and the declaration-handling bounds the parser depends
on.

Every audio fixture in this directory, and what it is for:

| Fixture | Shape | Exists to prove |
|---|---|---|
| `sine.wav` | 0.5 s, 44.1 kHz stereo PCM s16le | The baseline short track; tests that need a track to **end** use this length |
| `sine.mp3` | 0.5 s, 128 kbps MP3 | M1 promises MP3, and symphonia's `mp3` feature is not on by default |
| `sine.flac` | 0.5 s, FLAC | M1 promises FLAC |
| `sine-5s.flac` | 5 s, FLAC | Enough headroom to pause and play on past the landing without the fixture running out |
| `sine-5s.mp3` | 5 s, 128 kbps MP3 | H1 in MP3: audible playback while the body is still stalled mid-transfer |
| `sine-5s.wav` | 5 s, PCM s16le | H1 in WAV, same reason |
| `sine-5s.m4a` | 5 s, mono AAC, **tail-`moov`** | A tail-`moov` file opens over byte ranges and cannot open sequentially |
| `sine-5s.mka` | 5 s, stereo FLAC in Matroska | A supported container whose track time base (1/1000) is not the sample rate: seeks must convert the demuxer timestamp instead of reading it as frames |
| `sine-noxing.mp3` | 5 s, MP3, **no Xing/LAME header** | `Continuity::Unresolved` is reachable: nothing in the container or the transport declares a length |
| `sine-long-noxing.mp3` | 600 s, mono CBR MP3, no Xing | A rescan long enough to *time* (M3.1's seek wedge) |
| `sine-long-vbr-noxing.mp3` | 600 s, mono, genuinely VBR, no Xing | The byte-offset estimate's actual failure mode, not just its cost |
| `sine-22k-mono.mp3` | 5 s, 22.05 kHz mono MP3, no Xing | A reconnect whose decoder differs from the one it replaces |
| `sine-tagged.flac` | `sine.flac` with a `TITLE` tag carrying `ESC [2J` and a newline | `--probe-only` escapes decoder metadata instead of printing it raw |

M4 adds no audio fixture. `sine-5s.flac` is the one it reuses: `tests/m4_playback_identity.rs` and `tests/m4_cli.rs`'s probe test both serve it from the loopback server as a podcast episode's enclosure.

`sine.wav` — 0.5 s, 440 Hz, 44100 Hz, stereo, PCM s16le:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=0.5" \
      -ac 2 -c:a pcm_s16le sine.wav

`sine.mp3` and `sine.flac` are transcoded from `sine.wav` with
`-c:a libmp3lame -b:a 128k` and `-c:a flac` respectively.

`sine-tagged.flac` is `sine.flac` retagged:

    cp sine.flac sine-tagged.flac
    metaflac --remove-all-tags --set-tag=$'TITLE=Sine\x1b[2J\nInjected' sine-tagged.flac

MP3 and FLAC are present because M1 promises those formats. Testing WAV alone
would leave both promised codecs unexercised, and symphonia's `mp3` feature is
not enabled by default.

`sine-5s.flac` — 5 s, 440 Hz, 44100 Hz, stereo, FLAC from s16:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
      -ac 2 -sample_fmt s16 -c:a flac sine-5s.flac

The engine contract tests play a fixture, pause it, and then have to play on
past wherever the pause landed. How far that is depends on how full the PCM
ring was when the park took effect and on how far the harness clock ran before
the test thread next looked, both of which stretch under CPU contention. Half a
second of media leaves no room for that; five seconds leaves an order of
magnitude more than the worst drift observed. Tests that need a track to *end*
keep using `sine.flac`, which is short on purpose.

`sine-noxing.mp3` — 5 s, 440 Hz, 44100 Hz, stereo, MP3 with **no Xing/LAME
header**:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
      -ac 2 -c:a libmp3lame -b:a 64k -write_xing 0 sine-noxing.mp3

`-write_xing 0` is load-bearing, not cosmetic: ffmpeg's default mp3 mux writes
a Xing/LAME header carrying the frame count, which symphonia reads back as
`track.num_frames` — so a fixture regenerated with defaults would silently
self-declare a duration again. This is the one fixture here whose entire
purpose is *not* establishing a length: nothing in its container, and (served
without `Content-Length` or `Content-Range`) nothing in its transport, can
tell a reader how long it is. It exists to prove `Continuity::Unresolved` is
reachable at all — every other fixture here (WAV's `data` chunk size, FLAC's
STREAMINFO, an ordinary MP3's Xing header) self-declares a length one way or
another, which made that refusal path untestable before this fixture existed.

`sine-5s.mp3` and `sine-5s.wav` — 5 s, 440 Hz, 44100 Hz, stereo, the same
duration as `sine-5s.flac` in MP3 and WAV:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
      -ac 2 -c:a libmp3lame -b:a 128k sine-5s.mp3
    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
      -ac 2 -c:a pcm_s16le sine-5s.wav

H1 needs all three promised formats to play through test output while the
body is still stalled mid-transfer, which takes the same five seconds of
headroom `sine-5s.flac` exists for above — half a second leaves no room to
stall after enough bytes have arrived to be audible and still have more
body left to stall.

`sine-5s.m4a` — 5 s, 440 Hz, 44100 Hz, mono, AAC in an ISO-BMFF container
whose `moov` atom sits after the audio data (a tail-`moov` layout, ffmpeg's
default placement with no `-movflags +faststart`):

    ffmpeg -f lavfi -i "sine=frequency=440:duration=5" -c:a aac -b:a 64k sine-5s.m4a

Generated with ffmpeg 9.0.1. §12's closing paragraph asks for evidence that a
tail-`moov` file can be *opened* over HTTP ranges at all: without byte
seeking, a demuxer can never reach the `moov` atom describing the stream, so
a sequential (range-less) source is expected to fail here where a
range-capable one succeeds. Verified empirically (top-level box layout below,
dumped by walking the file's box headers) that `moov` sits after `mdat`
rather than in front of it, so the fixture actually exercises that layout
rather than the `faststart` one ffmpeg produces when asked:

    ftyp @ 0 (28 B), free @ 28 (8 B), mdat @ 36 (40512 B), moov @ 40548 (1630 B)

`an_m4a_recording_opens_over_ranges` asserts `SeekSupport::Unknown`, not
`Native`: byte-range support is evidence the *transport* can seek, not that
this container's demuxer can — every remote source starts at `Unknown` and
is only promoted by a trial seek that actually lands (§6). The point of this
fixture is narrower than that promotion: it proves the tail-`moov` file opens
at all, which a sequential source cannot do.

`sine-5s.mka` — 5 s, 440 Hz, 44100 Hz, stereo, FLAC in a Matroska container:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=5" \
      -ac 2 -sample_fmt s16 -c:a flac tests/fixtures/sine-5s.mka

Every other audio fixture here timestamps in samples, so its track time base
is `1/44100` and demuxer timestamps double as frame counts. Matroska
timestamps in the segment's own scale — milliseconds by spec default — so
this track's time base is `1/1000` (`ffprobe` reports `time_base=1/1000`
against `sample_rate=44100`). That divergence is the fixture's whole
purpose: `tests/decode_fixtures.rs`'s
`a_seek_on_a_container_whose_time_base_is_not_the_sample_rate_lands_on_target`
proves `seek_refined` converts the seek landing through the track's time
base instead of reading millisecond ticks as frames, which used to land a
3.5 s seek at ~1.6 s. Generated with ffmpeg 9.0.2.

`sine-22k-mono.mp3` — 5 s, 440 Hz, 22050 Hz, mono, MP3 with **no
Xing/LAME header**:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=22050:duration=5" -ac 1 \
      -c:a libmp3lame -b:a 64k -write_xing 0 tests/fixtures/sine-22k-mono.mp3

`sine-noxing.mp3`'s deliberate mismatch. M7 §7 lets a station come back on a
reconnect with entirely different audio parameters, and the engine has to
build conversion for the decoder it actually got rather than the one it
replaced — so `tests/m7_reconnect.rs` serves this 22.05 kHz mono body to the
connection that follows a 44.1 kHz stereo one. `-write_xing 0` is
load-bearing for the same reason it is on `sine-noxing.mp3`: a length the
container declares would make this finite, and a reconnect that comes back
finite is refused rather than played. Verified with `ffprobe` (22050 Hz, 1
channel, 5.07 s) and by byte search: no `Xing`, `Info` or `VBRI` marker
anywhere in the file.

`sine-long-noxing.mp3` — 600 s (10 min), 440 Hz, 44100 Hz, mono, MP3 with
**no Xing/LAME header**:

    ffmpeg -f lavfi -i "sine=frequency=440:duration=600" -c:a libmp3lame -b:a 128k \
      -write_xing 0 tests/fixtures/sine-long-noxing.mp3

`sine-noxing.mp3`'s sibling, at a length long enough for the cost of a
demuxer rescan to actually be measurable. Its whole existence is the M3.1
spike (`2026-09-10-tenuto-estimated-seek-design.md` §5.2): reproducing the
seek wedge needs a forward-scan expensive enough to time, and `sine-noxing.
mp3`'s 5 s gives a rescan nothing to walk across. `-write_xing 0` is
load-bearing for the same reason it is on `sine-noxing.mp3` — regenerating
with defaults writes back a Xing header carrying `num_frames`, which turns
this back into an ordinary indexed file and silently defeats the fixture's
purpose. Verified to carry no Xing/Info/VBRI marker anywhere in the file
(`grep`-equivalent byte search for `Xing`/`Info`/`VBRI`, none found) and to
still report `track.num_frames = Some(_)` on open — from
`estimate_num_mpeg_frames`'s bitrate arithmetic, never from a real index,
since nothing in this file provides one.

`sine-long-vbr-noxing.mp3` — 600 s, 44100 Hz, mono, MP3, genuinely variable
bitrate, **no Xing/LAME header**:

    ffmpeg -f lavfi -i "anoisesrc=color=white:amplitude=1:sample_rate=44100:duration=300" \
      -f lavfi -i "anullsrc=r=44100:cl=mono:d=300" \
      -filter_complex "[0:a][1:a]concat=n=2:v=0:a=1[out]" -map "[out]" \
      -c:a libmp3lame -q:a 0 -write_xing 0 tests/fixtures/sine-long-vbr-noxing.mp3

`sine-long-noxing.mp3` is CBR (single bitrate throughout), which makes
`estimate_num_mpeg_frames`'s byte-rate arithmetic *exact* — a fixture that
cannot exercise the estimate's actual failure mode, only its cost. This one
exists to requalify that spike's headline number: 300 s of white noise
(dense, ~417 B/frame) followed by 300 s of digital silence (~104 B/frame) —
about a 4x local bitrate ratio — chosen over a plain sine because a pure
tone compresses to an almost perfectly constant frame size even under `-q:a`
VBR (verified: a `sine`-only VBR attempt produced 22,967 of 22,970 frames at
one single byte size) and would not exercise anything a CBR fixture does
not. `-write_xing 0` is load-bearing for the same reason as on the other
`-noxing` fixtures. Verified to carry no Xing/Info/VBRI marker. Because the
sampled region (the first ~16 frames, all noise) is denser than the file's
second half, `estimate_num_mpeg_frames` extrapolates a duration of ~361 s —
*short* of the true 600 s — which is itself part of what this fixture is
for: it demonstrates that `MpaReader::seek`'s bounds check rejects any
target past the (wrong) estimated duration for *every* seek mode, not only
`Coarse`, and that ordinary forward playback is unaffected (`next_packet_ts`
advances from real frame durations, never clamped to `num_frames`). See
`docs/superpowers/specs/2026-09-10-tenuto-estimated-seek-design.md` §5.2
for the measurements this fixture produced.
