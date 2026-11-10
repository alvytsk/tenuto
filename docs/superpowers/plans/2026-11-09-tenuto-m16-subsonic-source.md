# Tenuto M16: A Subsonic source — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** save Subsonic-compatible servers (Subsonic/Ampache/Airsonic/Gonic/Navidrome), browse artists → albums → tracks in a fourth browser tab, queue and play server tracks with the engine untouched, checkpoints keyed by stable identity.

**Spec:** [`../specs/2026-11-09-tenuto-m16-subsonic-source-design.md`](../specs/2026-11-09-tenuto-m16-subsonic-source-design.md).

**Architecture:**
- Four PR-shaped stages, store first: (1) `server/` store + CLI, (2) the API client + resolution, (3) browser tab, (4) diagnostics + docs.
- The engine never learns Subsonic exists: resolution is pure URL construction to `SourceLocation::Http`.

**Branch:** `feature/m16-subsonic`, from `main`.

---

## Task 1: `src/server/` store and CLI

- [ ] `store.rs`: `servers.json` + `servers.lock` mirroring `stations.json`'s lifecycle exactly (atomic writes, quarantine, contention refuses); `server_id` stable across re-add with an existing name.
- [ ] `servers` / `servers add <base-url> --as <name> --user <u>` (hidden tty password prompt; only `password_md5` stored; `--password` prints the history warning) / `servers remove <slug>` (y-confirmed).
- [ ] Register the module at Sources rank 1 in `src/lib.rs` + `tests/m9_layering.rs`'s `LAYERS`.
- [ ] Suite part 1 of `tests/m16_servers.rs`: store lifecycle, lock contention, re-add stability, removal deletes cache and keeps checkpoints.

## Task 2: API client and resolution

- [ ] `api.rs`: request construction (`u`/`t`/`s`/`c=tenuto`/`v=1.16.0`, fresh salt per request) and `quick-xml` response parsing for `ping`, `getArtists`, `getArtist`, `getAlbum`, `getCoverArt` ids; `resolve.rs`: pure `server:<server-id>/<track-id>` → stream/cover URLs; golden md5 token vectors.
- [ ] Malformed XML reduces to a category (`ServerError` after the `FeedError` pattern); undecodable documents refused, not repaired.
- [ ] `library.rs` gains `list_servers`, `connect_server`, `list_artists`, `list_albums`, `list_tracks`; the browse worker routes the navigation rounds; per-album cache under `servers/<server-id>/` with the 24 h expiry.
- [ ] Queue entries: `QueueSource` server variant; runtime resolves `server:` identities to sources on the PlayEntry path (the episode-enclosure precedent); suite part 2: identity stability across salt rotation, add → restart → resume keyed by `server:` identity.

## Task 3: The Servers tab

- [ ] Fourth browser tab; Files-tab navigation keys; Enter adds tracks to the opening playlist; `r` re-pings; covers fetched on the artwork worker only when displayed, within the existing caps.
- [ ] Suite part 3: browser flows through the runtime harness against the fixture Subsonic server (the test HTTP harness grows the endpoint set with fixture audio), cache expiry, cover-after-display.

## Task 4: Diagnostics, docs, gate

- [ ] `m4_diagnostics.rs` audit extension: token-bearing URLs never appear whole in messages or logs, including bad-password/403/unreachable paths.
- [ ] `docs/reference.md` (commands, tab, identity/checkpoint rule); `docs/architecture.md` §2, §3, §4, §8 (third MediaId scheme), §12 revision; `CHANGELOG.md`; acceptance script against a real Navidrome recorded in `docs/m16-acceptance.md`.
- [ ] Full gate: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked --no-fail-fast && RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`.
