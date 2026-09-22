source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/check-tag.sh"

expect_exit 0 bash "$s" v0.1.3 0.1.3
expect_exit 1 bash "$s" 0.1.3 0.1.3
expect_contains "expected 'v0.1.3'" "$ERR" "missing v message"
expect_exit 1 bash "$s" v0.1.3-rc1 0.1.3
expect_exit 1 bash "$s" v0.1.4 0.1.3
expect_exit 2 bash "$s" v0.1.3

finish
