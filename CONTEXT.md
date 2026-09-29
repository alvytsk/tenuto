# tenuto

A keyboard-first terminal audio player for local files, remote audio over HTTP, live radio and podcast episodes. This glossary fixes the words the code, the docs and design discussions use for its domain.

## Language

### Playback

**Media**:
One playable thing, known by its identity: a local file, a URL, a podcast episode or a radio station.
_Avoid_: track (except in user-facing text), item

**Position**:
The session's logical resume point in the current media. Stopping or recreating the transport never resets it.
_Avoid_: offset, playhead

**Checkpoint**:
The stored position for one media, kept across restarts.
_Avoid_: bookmark, save point

**Current media**:
The media the session is on. There are two copies, and when they can differ, name the one you mean.
- *Session's current media* follows loads.
- *Persisted current media* is restored on restart, but never played automatically.

Deleting the playing playlist moves only the persisted copy: to the successor's cursor, or to none when the successor has no cursor. Recovery either clears a cursor that disagrees with the persisted copy, or, when it has to pick a new playing playlist, moves the persisted copy to that playlist's cursor.
_Avoid_: now playing, resume media

**Owned entry**:
The entry whose load the session has adopted. Removing it is the only playlist edit that stops playback.
_Avoid_: active entry, current entry

### Playlists

**Playlist**:
A named, ordered list of entries the listener plays from. There is always at least one.
_Avoid_: queue (the pre-M8 name for the single list)

**Entry**:
One occurrence of a media in a playlist. Its identity is unique across every playlist, so the same media may appear as several entries.
_Avoid_: item, row (a row is what the player draws)

**Cursor**:
A playlist's remembered entry, where playback in that playlist continues. A cursor is not ownership.
_Avoid_: active entry, selection

**Playing playlist**:
The playlist that playback, next, previous and advance follow. Requesting a load never changes it; adopting one does, and so do deleting the playing playlist and recovery.
_Avoid_: current playlist

**Viewed playlist**:
The playlist the player shows and edits act on. It is never stored.
_Avoid_: selected playlist, open tab

**Playlist set**:
All of the listener's playlists, which one is playing, and the rules that bind them together.
_Avoid_: library (that word belongs to feeds and stations)

**Playback order**:
The order that next, previous and advance follow: the playlist's own order, or its shuffled order.
_Avoid_: play order, sequence

**Shuffle**:
A per-playlist hidden playback order. It leaves the displayed order alone and always starts from the cursor.
_Avoid_: random

**Successor**:
The playlist that takes a deleted playlist's place: the next one in the strip, or the previous one if the deleted playlist was last.
_Avoid_: fallback, neighbour
