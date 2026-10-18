# M11 Desktop media controls — acceptance

Scope: MPRIS media controls for `tenuto tui` and `tenuto play`
(`docs/superpowers/specs/2026-10-18-tenuto-m11-mpris-design.md`). Linux only.
The widget shows the title, artist and length of what plays, and play, pause,
stop and volume work from it.

Branch `feat/m11-mpris`, from `main` at `8875817`.

## Automated evidence

| Area | Where | Tests |
|---|---|---|
| Snapshot, request mapping, change detection | `src/application/mpris/mod.rs` | `snapshot_maps_each_phase`, `snapshot_omits_length_for_live_and_unknown_durations`, `snapshot_prefers_podcast_name_over_tag_artist`, `snapshot_strips_nul_from_title_and_artist`, `snapshot_without_now_playing_has_no_track`, `command_maps_transport_requests`, `command_volume_rejects_nan_and_clamps`, `click_flood_drops_beyond_backlog`, `first_apply_reports_everything`, `new_media_bumps_track_id`, `position_only_change_reports_nothing_but_updates_position`, `same_media_new_length_keeps_track_id_and_reports_metadata`, `status_only_change_reports_playback_status`, `volume_only_change_reports_volume`, `volume_write_reports_volume_even_when_unchanged`, `track_id_is_no_track_without_a_track` |
| D-Bus interface objects | `src/application/mpris/bus.rs` | `pause_queues_pause_not_toggle`, `set_volume_queues_and_marks_written`, `set_volume_rejects_nan`, `navigation_and_seek_are_ignored`, `position_reads_the_latest_snapshot`, `metadata_has_no_track_without_a_track`, `a_panic_on_the_thread_is_contained` |
| Runtime and pause race | `tests/m11_mpris.rs` | `a_pause_after_the_engine_paused_does_not_resume`, `pause_does_nothing_when_nothing_is_loaded`, `pause_on_a_paused_station_leaves_it_paused`, `a_play_pause_request_reaches_the_runtime`, `the_snapshot_describes_the_playing_track`, `a_click_with_nothing_loaded_does_nothing`, `a_podcast_episode_shows_the_podcast_name_as_artist`, `position_advances_in_successive_snapshots` |
| `play` shows tag title and artist | `tests/m9_3_detached_play.rs` | `a_detached_tagged_file_shows_its_tag_title_and_artist` |
| Cover art: writer, hand-off, fetch | `src/application/mpris/cover.rs`, `src/application/mpris/mod.rs` | `cover_images_compare_by_pointer`, `write_cover_caps_the_long_edge_and_keeps_aspect`, `write_cover_keeps_a_small_image_size`, `clear_covers_removes_only_cover_files`, `a_stale_result_never_becomes_the_cover`, `same_media_new_cover_reports_metadata_and_keeps_track_id`, `take_cover_update_reports_each_change_once` |
| Cover art: bus `artUrl` | `src/application/mpris/bus.rs` | `a_new_cover_is_written_and_published_as_a_file_url`, `the_next_cover_deletes_the_previous_file`, `a_cleared_cover_deletes_the_file_and_drops_art_url`, `an_unwritable_cover_dir_leaves_no_art_url` |
| Cover art: runtime | `tests/m11_mpris.rs` | `a_playlist_track_with_an_embedded_picture_publishes_a_cover`, `a_detached_track_with_an_embedded_picture_publishes_a_cover`, `an_untagged_track_publishes_no_cover`, `the_previous_cover_is_dropped_on_track_change`, `a_cover_that_fails_to_decode_publishes_none`, `a_large_embedded_cover_reaches_the_snapshot_shrunk` |
| Hung or missing bus | `tests/m11_mpris_process.rs` | `play_runs_and_exits_with_an_unresponsive_bus`, `play_without_a_bus_writes_nothing_new_to_stderr` |

The subprocess and PTY harnesses set `TENUTO_MPRIS=off`; the two process
tests above remove it.

## Manual check (pending, user)

In the niri bar, for both `tui` and `play`:

- [ ] A tagged local track, a podcast episode and a radio station each show the right title and artist.
- [ ] Play/pause from the widget works and the widget's state follows the space key; Next/Previous don't appear.
- [ ] A tagged track, a Radio-T episode and a station each show their cover in the bar (spec §10.4); an untagged track shows none, and `~/.cache/tenuto/mpris/` holds at most one `cover-<pid>-<n>.png` per running instance, earlier runs' files removed at startup, the last cover removed on exit when possible.
- [ ] The widget entry goes away on quit.
- [ ] With `DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent`, tenuto starts normally and writes nothing to the terminal (`tui` logs one warning to its log file).
- [ ] With `TENUTO_MPRIS=off`, tenuto doesn't appear in the widget.
