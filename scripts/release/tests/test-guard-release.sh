source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/guard-release.sh"
export PATH="$FIXTURES/bin:$PATH"

STUB_GH_RELEASE=none expect_exit 0 bash "$s" v0.1.3
STUB_GH_RELEASE=draft expect_exit 1 bash "$s" v0.1.3
expect_contains "incomplete draft" "$ERR" "draft message"
STUB_GH_RELEASE=published expect_exit 1 bash "$s" v0.1.3
expect_contains "never overwritten" "$ERR" "published message"
STUB_GH_RELEASE=error expect_exit 2 bash "$s" v0.1.3
expect_exit 2 bash "$s"

finish
