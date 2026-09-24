#!/usr/bin/env bash
# changelog-section.sh <version> [changelog]: print the body of
# "## [<version>] - YYYY-MM-DD", up to the next "## " heading or the link
# reference block, without the heading (Linux packages spec §7.3).
# Hard-wrapped lines are joined: a GitHub release body keeps every newline
# as a line break, so a wrapped paragraph would render as a narrow column.
# Exit 0 printed, 1 missing/duplicated/empty section, 2 usage.
set -euo pipefail
if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  echo "usage: changelog-section.sh <version> [changelog]" >&2
  exit 2
fi
version=$1
file=${2:-CHANGELOG.md}
if [ ! -f "$file" ]; then
  echo "changelog-section: $file does not exist" >&2
  exit 1
fi
heading="## [$version] - "
date_re='^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$'

count=$(awk -v h="$heading" -v re="$date_re" '
  index($0, h) == 1 && substr($0, length(h) + 1) ~ re { n++ }
  END { print n + 0 }' "$file")
if [ "$count" -eq 0 ]; then
  echo "changelog-section: no section '## [$version] - YYYY-MM-DD' in $file" >&2
  exit 1
fi
if [ "$count" -gt 1 ]; then
  echo "changelog-section: $count sections for $version in $file" >&2
  exit 1
fi

# unwrap: join each continuation line onto the line before it. Blank lines,
# headings, list items, quotes, table rows and fenced code start a new line;
# a line ending in two spaces or a backslash keeps its hard break.
unwrap() {
  awk '
    function flush() { if (have) print cur; have = 0 }
    /^[[:space:]]*(```|~~~)/ { flush(); print; fence = !fence; next }
    fence { print; next }
    /^[[:space:]]*$/ { flush(); print; next }
    /^[[:space:]]*([-*+]|[0-9]+[.)])[[:space:]]/ || /^[[:space:]]*(#|>|\|)/ {
      flush(); cur = $0; have = 1; brk = /(  |\\)$/; next
    }
    have && !brk { t = $0; sub(/^[[:space:]]+/, "", t); cur = cur " " t; brk = /(  |\\)$/; next }
    { flush(); cur = $0; have = 1; brk = /(  |\\)$/ }
    END { flush() }'
}

body=$(awk -v h="$heading" -v re="$date_re" '
  on && (/^## / || /^\[[^]]+\]: /) { exit }
  on { print }
  index($0, h) == 1 && substr($0, length(h) + 1) ~ re { on = 1 }' "$file" \
  | sed '/./,$!d' | unwrap)
if [ -z "${body//[[:space:]]/}" ]; then
  echo "changelog-section: the $version section in $file is empty" >&2
  exit 1
fi
printf '%s\n' "$body"
