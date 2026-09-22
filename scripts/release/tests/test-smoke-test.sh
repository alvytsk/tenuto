source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/smoke-test.sh"
stub="$FIXTURES/bin-tenuto/tenuto"
wav="$RELEASE_DIR/../../tests/fixtures/sine.wav"

expect_exit 0 bash "$s" "$stub" 1.2.3 "$wav"
STUB_TENUTO_MODE=wrong-version expect_exit 1 bash "$s" "$stub" 1.2.3 "$wav"
expect_contains "--version" "$ERR"
STUB_TENUTO_MODE=writes-home expect_exit 1 bash "$s" "$stub" 1.2.3 "$wav"
expect_contains "not empty" "$ERR"
STUB_TENUTO_MODE=no-ended expect_exit 1 bash "$s" "$stub" 1.2.3 "$wav"
expect_contains "[ended]" "$ERR"
STUB_TENUTO_MODE=no-state expect_exit 1 bash "$s" "$stub" 1.2.3 "$wav"
expect_contains "state.json" "$ERR"
expect_exit 2 bash "$s" "$stub" 1.2.3

# A caller's TENUTO_AUDIO_OUTPUT must not leak into steps 1-4; play sets its own.
TENUTO_AUDIO_OUTPUT=cpal expect_exit 0 bash "$s" "$stub" 1.2.3 "$wav"

finish
