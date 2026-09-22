#!/usr/bin/env bash
# guard-release.sh <tag>: proceed only if no release exists for the tag
# (Linux packages spec §7.4). Exit 0 none, 1 one exists, 2 gh failed.
set -uo pipefail
if [ $# -ne 1 ]; then
  echo "usage: guard-release.sh <tag>" >&2
  exit 2
fi
tag=$1
err=$(mktemp)
trap 'rm -f "$err"' EXIT

if out=$(gh release view "$tag" --json isDraft 2>"$err"); then
  case "$(jq -r .isDraft <<<"$out" 2>/dev/null)" in
    true)
      echo "guard-release: an incomplete draft for $tag exists; inspect and delete it, then rerun" >&2
      exit 1 ;;
    false)
      echo "guard-release: $tag is already published; a published release is never overwritten" >&2
      exit 1 ;;
    *)
      echo "guard-release: unexpected gh output: $out" >&2
      exit 2 ;;
  esac
fi
if grep -q "release not found" "$err"; then
  echo "guard-release: no release for $tag yet; proceeding"
  exit 0
fi
echo "guard-release: gh release view failed:" >&2
cat "$err" >&2
exit 2
