# M10 Finite HTTP recovery — acceptance

Scope: finite-media recovery of roadmap M10
(`docs/superpowers/specs/2026-09-29-tenuto-m10-finite-reconnect-design.md`).
A remote finite episode with `Supported` resume that drops or stalls mid-play
reconnects through `Reconnecting`, `Outage` and `ReconnectPolicy`, and resumes
at the heard position.

Branch `feature/finite-reconnect`, from `main` at `4fb5245`.

## Gate

Run on this branch after the last code change (`3ded602`); the commits after
it touch documentation only.

| Check | Command | Result |
|---|---|---|
| Format | `cargo fmt --check` | exit 0, no output |
| Lints | `cargo clippy --locked --all-targets --all-features -- -D warnings` | finished with no warnings |
| Tests | `cargo test --locked --no-fail-fast` | exit 0; 107 result lines (integration binaries, unit-test binaries and doc-tests): 1421 passed, 0 failed, 3 ignored |
| Docs | `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` | finished with no warnings |

The 3 ignored are the intentional ones: `device_smoke` and the two
`m8_snapshot_size` measurements.

## Unchanged suites

`git diff main -- tests/m7_*.rs tests/m4_diagnostics.rs` prints nothing.

## Spec §9 tests

All names are in `tests/m10_finite_reconnect.rs` unless a path is given.

1. Audio continuity: `a_dropped_connection_resumes_with_no_frame_repeated_or_skipped`
   (amended: one seam replays `UNHEARD_FRAMES`, consecutive on both sides, spec §9).
   Frame conversion: `decode::tests::a_position_converted_from_frames_converts_back_to_the_same_frame`.
2. `TruncatedBody` on every path: `a_truncated_reopen_or_reseek_is_retried_inside_the_same_outage`;
   classifier: `reconnect::tests::retry_classification_follows_continuity`.
3. Not eligible: `a_range_less_server_still_fails`, `an_episode_never_proved_seekable_still_fails`;
   proof carried across a reopen: `a_reopen_keeps_a_seek_the_session_already_proved`.
4. Seek while reconnecting: `a_seek_during_recovery_is_stored_offline_and_the_landing_anchors_at_it`,
   `seek_by_presses_during_recovery_accumulate`,
   `a_seek_near_the_end_during_recovery_plays_out_and_ends`.
5. Pending intent survives failure: `a_failed_attempt_keeps_the_stored_target_for_the_next`,
   `a_failed_resume_keeps_the_stored_target_for_the_next_play` (restore).
5a. Landing is the anchor: `a_seek_during_recovery_is_stored_offline_and_the_landing_anchors_at_it`.
5b. Priming failure is the attempt's: `a_failure_while_priming_is_the_attempts_not_the_sessions`,
   `a_priming_failure_counts_against_the_budget`,
   `a_priming_failure_on_space_fails_honestly_and_keeps_the_target` (restore),
   `a_device_that_will_not_open_fails_the_attempt_at_once`.
6. Restart intent: `a_restart_during_recovery_lands_at_zero_as_a_restart`,
   `a_seek_after_a_restart_supersedes_it`,
   `a_restart_stored_during_recovery_survives_stop_and_space`,
   `a_restart_stored_during_recovery_survives_pause_and_space`,
   `a_restart_whose_priming_drops_is_landed_by_the_recovery`.
7. Pause and stop: `pausing_during_recovery_keeps_the_target_and_space_resumes_at_it`,
   `stopping_during_recovery_keeps_the_target_and_space_resumes_at_it`,
   `a_seek_in_a_recovery_pause_is_stored_offline_and_space_lands_at_it`,
   `pause_cancels_a_blocked_reopen`, `pause_cancels_a_blocked_reseek`,
   `pause_cancels_a_blocked_priming_read`, `stop_cancels_a_blocked_priming_read_and_space_resumes`,
   `a_pause_raced_by_a_dropped_connection_lands_paused_not_reconnecting`,
   `a_new_load_during_recovery_drops_the_outage_and_the_target`,
   `shutdown_during_a_blocked_attempt_joins_promptly`.
   Session checkpoint of the stored target: `tests/session_policy.rs`
   (`a_stopped_seek_target_survives_the_shutdown_force`,
   `a_resumed_stopped_seek_clears_the_target_through_its_seek_completed`,
   `a_media_switch_carries_the_outgoing_stopped_seek_target_out_with_it`).
7a. Arrow bursts: `arrow_bursts_during_recovery_accumulate_across_progress`
   and `seek_by_while_stopped_accumulates_on_the_stored_target` (integration);
   `application::seek` unit tests `a_stored_target_holds_the_display_and_seeds_the_next_burst`,
   `the_stored_target_is_the_workers_clamped_one`, `the_landing_releases_a_stored_target`,
   `a_restart_landing_releases_a_stored_target`, `a_stop_drops_the_burst_but_keeps_the_stored_target`.
8. Budget: `past_the_budget_it_fails_and_space_tries_exactly_once`,
   `reconnect::tests::the_budget_is_judged_only_when_something_fails`.
9. Heard-time window: `a_forward_seek_does_not_end_the_outage`,
   `a_backward_seek_does_not_stop_heard_playback_ending_the_outage`;
   arithmetic and ring drain (decision 7): `reconnect::tests::short_connections_stay_one_outage_and_thirty_heard_seconds_end_it`,
   `heard_time_counts_only_once_a_reconnect_is_playing`,
   `a_failure_closes_the_window_and_forgets_what_was_heard`.

## Known issues

Pre-existing, out of M10's scope:
- The live path drops its source while attempting (`source_ended`), so
  `fresh_open` reads `Cancelled` and a station dying during priming retries
  without backoff or budget. The finite path was fixed; the m7 suites had to
  stay unchanged.

Deferred minors from the review ledger:
- `pause()`'s rebuild guard (state changed to `Reconnecting`/`Failed`) has no
  test; the harness has no park-only fault.
- The 16 KiB stall in the blocked-priming pause/stop tests is not confirmed to
  land in priming rather than in the reseek.
- The exact commit-versus-pause race is untested.
- Spec §7's "a seek during an in-flight attempt cancels it, and the next
  attempt runs at once with no backoff spent" is untested.
- `lost_source_while_playing` also closes a pause queued before a seek on a
  healthy remote episode; Space then costs one extra reopen. Untested and
  documented only in architecture §7.6.

## Manual check (pending, user)

Build release, play a Radio-T episode in Ghostty, toggle the VPN mid-play, and
confirm the player shows Reconnecting and then resumes at the same moment.
