# Tenuto M16: A Subsonic source (media server library)

Status: drafted on 2026-11-09 from a feature conversation. It is ready for an implementation plan. The code does not yet match it.

Context:
- The largest item of the Winamp survey: the modern successor of Winamp's networked media library. Tenuto's context diagram already has "Media servers" (§2) as a source of finite HTTP audio; this milestone deepens that container from "a URL you paste" to "a library you browse".
- One protocol, chosen deliberately: the **Subsonic API** (v1.16.0), which also covers Ampache, Airsonic-Advanced, Gonic and Navidrome through their Subsonic-compatible endpoints. Jellyfin has a different API and is recorded as the natural follow-up (§8), not smuggled into this milestone.

## 1. Decision

A fourth saved-resource kind beside feeds and stations: **servers**. A server store (`servers.json` in `$XDG_DATA_HOME/tenuto`, behind `servers.lock`, mirroring `stations.json`'s file lifecycle exactly: atomic writes, quarantine, contentions refuse at once) holds `{server_id, name, base_url, username, password_md5, added}`.

### 1.1 Identity and resolution

- New canonical `MediaId` scheme: `server:<server-id>/<track-id>` — the third scheme beside `local:`/`remote:`/`podcast:` (§8). The track-id is the server's own track id, opaque here, byte-for-byte as the API returns it.
- Identity never derives from a URL, so the token/salt in request URLs cannot break identity (the rule that survives CDN moves for podcasts survives salt rotation here). What plays and what is checkpointed stay different values on purpose, exactly as §8 prescribes.
- Resolution at load time is **pure URL construction, no network**: Subsonic auth is `u`, `t` (token = `md5(hex(md5(password)) + salt)`), `s` (a fresh random salt per request), `c=tenuto`, `v=1.16.0`. The runtime resolves a `server:` entry to `SourceLocation::Http` the same way it looks up an episode's enclosure today — one function in the new module, called from the PlayEntry path. Storing only `password_md5` (never the password) is what makes token construction possible; the spec requires the Subsonic hex-md5 auth path precisely for this.
- The stream URL (`stream` endpoint) targets a finite, range-capable resource on every mainstream server, so seek, resume, reconnect (M10) and the existing HTTP classification all apply unchanged. Nothing in the engine learns that Subsonic exists.

### 1.2 Browsing

The browser (Files / Podcasts / Radio) gains a **Servers** tab: server list → artists (`getArtists`) → artist (`getArtist`) → albums → album tracks. Navigation keys are the Files tab's; Enter on a track adds it to the playlist the browser opened on, exactly as a station or episode adds today.

- All fetches run on the browse worker through the library layer (`library` stays the application seam: `list_servers`, `connect_server`, `list_artists`, `list_albums`, `list_tracks`), one request per user navigation step — the invariant "nothing fetches on its own; every request follows a user action" holds by construction, the same way podcast browsing does.
- Metadata is cached per album in the server's cache directory (`servers/<server-id>/`, disposable, refetchable — the feed-cache precedent), so re-entering an album does not re-fetch. No validators are stored in this milestone; a re-entry refetches after cache expiry (a fixed 24 h), stated plainly.
- Cover art: album covers display in the browser list the way feed artwork displays for podcasts — fetched on the artwork worker only when displayed, within the existing caps. The playing track's cover reaches `PlayerView`/MPRIS through the existing `Covers` path by resolving `getCoverArt` with a fresh token.
- `r` on a server re-pings it (`ping` endpoint) and reports version/autenticity — the station re-probe's analog.

### 1.3 Account management

- CLI: `servers` lists; `servers add <base-url> --as <name> --user <username>` prompts for the password on the tty (hidden read, no echo) and stores only its md5 hex; `servers remove <slug>` after a `y` confirmation; `--password` exists for scripts but prints a one-time warning that the value lands in shell history.
- The redaction rules apply with teeth here: server URLs and `t`/`s` parameters are secret-bearing; every message and log line passes `redact_url`, and `tests/m4_diagnostics.rs`'s audit gains server-shaped URLs (token-bearing stream and cover URLs must never appear whole).
- Removing a server removes its cache; checkpoints are kept, like `unsubscribe` (they orphan, and that is fine and stated).

### 1.4 Persistence and queue entries

- `QueueSource` gains a server variant carrying `(server_id, track_id)`; queue entries display "artist — title" from the fetched metadata the way podcast episodes display theirs, and survive restarts as identity, not URL — a playlist made of server tracks still plays after the server's URL changes, provided the server id and track id resolve (a moved server is re-pointed by re-adding with the same slug; the store keeps `server_id` stable across `servers add` with an existing name — stated rule, tested).
- The checkpoint map needs no changes (keyed by `MediaId`).

## 2. What exists today (verified)

- `SourceLocation` is `LocalPath | Http` (`src/media/source.rs`) — the stream URL lands in the existing `Http` variant; no engine-facing type changes at all.
- The runtime's PlayEntry path already resolves identities to sources before `Load` (the episode-enclosure lookup in `application/runtime.rs`) — the resolution seam this feature extends.
- `station/` is the template for a store + slug rules + lock + probe; `subscription/` for cache lifecycle; `feed/` for fetch-and-parse with typed errors (`FeedError` pattern → a `ServerError`).
- The browser's three-tab structure and its worker round-trips (`tui/browser.rs`, `tenuto-browse`).
- The layering test: a new top-level module `server` lands at Sources rank 1 (beside `station`/`subscription`), registered in `src/lib.rs` and the test's `LAYERS`.

## 3. Module layout

- `src/server/` — `mod.rs` (types), `store.rs` (`servers.json` + lock), `api.rs` (the Subsonic client: request construction, response parsing — `quick-xml` as feeds do, bytes decide encoding), `resolve.rs` (MediaId → stream/cover URLs, pure).
- `library.rs` gains the server functions; `application/browse` gains the server navigation rounds; `tui/browser.rs` the fourth tab.
- No engine or playback changes. `http/` is used as-is. Tokio work stays inside the existing one-worker runtime and the browse worker's `wait_http` bridge.

## 4. Commands and keys

| Where | Input | Action |
|---|---|---|
| CLI | `servers`, `servers add …`, `servers remove <slug>` | manage the server list |
| Browser | Servers tab; arrows/Tab/Enter/Space as the other tabs | browse artists → albums → tracks; add tracks |

No new keys in the main list. The `?` help and `docs/reference.md` gain the tab and commands.

## 5. Tests

- `tests/m16_servers.rs` — against a fixture Subsonic server (the test HTTP harness grows the handful of endpoints: `ping`, `getArtists`, `getArtist`, `getAlbum`, `stream`, `getCoverArt`, serving fixture audio): add/list/remove with the lock contention behavior; token construction is deterministic given salt (golden md5 vectors); MediaId stability across salt rotation; add-to-playlist, restart, resume (checkpoint keyed by `server:` identity survives a URL change); remove-server orphans checkpoints and deletes cache.
- `tests/m16_servers_diagnostics.rs` — or a section in `m4_diagnostics.rs`: token-bearing URLs never appear whole in messages or logs, including in error paths (bad password, 403, unreachable host).
- Browser flows through the runtime harness: navigation rounds, cache expiry, cover fetch after display, the redaction of the browser's own status lines.
- The parsing layer gets the feed suite's treatment: malformed XML reduces to a category, undecodable documents are refused, not repaired.

## 6. Documentation

- `docs/reference.md`: the `servers` commands, the Servers tab, the identity/checkpoint rule ("a server track's position survives a URL change; removing a server keeps checkpoints but they can no longer resolve").
- `docs/architecture.md`: §2 context (media servers named Subsonic-compatible), §3 containers (`servers.json`, `servers.lock`, the server cache), §4 the `server` component row and the import edges (`library → server`, `runtime → server` for resolution — runtime already imports `station`), §8 the third MediaId scheme, §12 the scope revision ("media servers" leaves the out-of-scope line: the decision dated and one-lined).
- `CHANGELOG.md` at implementation time.

## 7. Acceptance

Manual script, recorded in `docs/m16-acceptance.md`, against a real Navidrome (or any Subsonic) instance: add with prompted password; browse to an album; queue several tracks; play, seek, drop the connection mid-track and watch M10 recovery; restart and resume; rotate the salt/token freely (identity holds); remove the server and confirm the polite orphaning. Transcoding (a server configured to transcode) plays as finite HTTP as long as ranges survive; a range-less transcode profile degrades exactly as the HTTP table in `docs/reference.md` already states — no special case is added.

## 8. Out of scope

- **Jellyfin** (different API, auth and item model): the natural M17; the `server` module's `api.rs` is the seam it would extend, and `resolve.rs` already separates identity from URL.
- Offline caching of server media (download-for-later), scrobbling/last.fm submission, playlist sync *to* the server, and admin operations — all server-write or sync concerns beyond "browse and play".
- storing OAuth/API keys beyond the md5 scheme (Subsonic's API offers exactly this one hash path; Jellyfin's tokens are the follow-up's problem).
- Background sync of the library: browsing is on-demand by the invariant; a "refresh library" bulk op is deferred until the cache-expiry story proves annoying.

## 9. Risks

- The Subsonic API's wild ecosystem: server implementations differ in musl details (id formats, cover-art ids, indexing). The fixture server pins the contract tenuto honors; the acceptance script on a real Navidrome is the reality check; the spec's stance is "refuse the odd, never repair" as feeds do.
- md5-for-auth is the API's design, not tenuto's choice — stated in the docs so nobody audits the repo for "password crypto" and finds md5 without the context. The password itself is never stored; the md5 is of the password, per the API's hex-md5 scheme.
- A server track added to a playlist while offline: the queue entry persists (identity), the load fails with the ordinary remote-failure path. Accepted and documented; no offline resolution.
- This milestone's breadth (store + api + browser tab + CLI) is deliberately the largest in the set; the plan splits it into four reviewable PR-shaped stages, store first.
