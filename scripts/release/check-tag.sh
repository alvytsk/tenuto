#!/usr/bin/env bash
# check-tag.sh <tag> <version>: the tag must be exactly "v" + the Cargo.toml
# version (Linux packages spec §7.1). Exit 0 match, 1 mismatch, 2 usage.
set -euo pipefail
if [ $# -ne 2 ]; then
  echo "usage: check-tag.sh <tag> <version>" >&2
  exit 2
fi
tag=$1
version=$2
if [ "$tag" != "v$version" ]; then
  echo "check-tag: tag '$tag' does not match Cargo.toml version '$version' (expected 'v$version')" >&2
  exit 1
fi
echo "check-tag: $tag matches Cargo.toml $version"
