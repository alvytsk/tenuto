source "$(dirname "$0")/../lib.sh"
s="$RELEASE_DIR/changelog-section.sh"

expect_exit 0 bash "$s" 0.2.0 "$FIXTURES/CHANGELOG.ok.md"
expect_equal $'### Fixed\n\n- A fix in 0.2.0.' "$OUT" "0.2.0 body"

# The last section stops at the link references, not at EOF.
expect_exit 0 bash "$s" 0.1.0 "$FIXTURES/CHANGELOG.ok.md"
expect_equal $'### Added\n\n- First release.' "$OUT" "0.1.0 body"

expect_exit 1 bash "$s" 0.3.0 "$FIXTURES/CHANGELOG.ok.md"
expect_contains "no section" "$ERR"
expect_exit 1 bash "$s" Unreleased "$FIXTURES/CHANGELOG.ok.md"
expect_exit 1 bash "$s" 0.2.0 "$FIXTURES/CHANGELOG.dup.md"
expect_contains "2 sections" "$ERR"
expect_exit 1 bash "$s" 0.2.0 "$FIXTURES/CHANGELOG.empty.md"
expect_contains "empty" "$ERR"
# A dot is not a wildcard: 0x2x0 must not match 0.2.0.
expect_exit 1 bash "$s" 0x2x0 "$FIXTURES/CHANGELOG.ok.md"
expect_exit 1 bash "$s" 0.2.0 "$FIXTURES/does-not-exist.md"
expect_exit 2 bash "$s"

finish
