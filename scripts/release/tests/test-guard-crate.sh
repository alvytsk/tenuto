source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/guard-crate.sh"
export PATH="$FIXTURES/bin:$PATH"

STUB_CRATE=absent expect_exit 0 bash "$s" tenuto 0.1.3
expect_contains "proceeding" "$OUT" "absent message"

STUB_CRATE=present expect_exit 1 bash "$s" tenuto 0.1.3
expect_contains "already on crates.io" "$ERR" "present message"

# A yank hides a version but never frees its number, so the guard must
# refuse a yanked version exactly as it refuses a live one.
STUB_CRATE=yanked expect_exit 1 bash "$s" tenuto 0.1.3
expect_contains "already on crates.io" "$ERR" "yanked message"

STUB_CRATE=error expect_exit 2 bash "$s" tenuto 0.1.3
STUB_CRATE=curlfail expect_exit 2 bash "$s" tenuto 0.1.3

expect_exit 2 bash "$s"
expect_exit 2 bash "$s" tenuto

finish
