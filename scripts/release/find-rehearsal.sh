#!/usr/bin/env bash
# find-rehearsal.sh <sha>: is there a successful workflow_dispatch run of
# release.yml on main for exactly this commit? (Linux packages spec §7.2)
# Exit 0 found (prints its URL), 1 none found, 2 the lookup itself failed.
set -uo pipefail
if [ $# -ne 1 ]; then
  echo "usage: find-rehearsal.sh <sha>" >&2
  exit 2
fi
sha=$1
if ! [[ "$sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "find-rehearsal: '$sha' is not a full 40-character SHA" >&2
  exit 2
fi
if [ -z "${GITHUB_REPOSITORY:-}" ]; then
  echo "find-rehearsal: GITHUB_REPOSITORY is not set" >&2
  exit 2
fi

# gh workflow view has no --json flag; the REST API gives the workflow id.
if ! workflow_id=$(gh api "repos/$GITHUB_REPOSITORY/actions/workflows/release.yml" --jq .id); then
  echo "find-rehearsal: could not read the release.yml workflow id" >&2
  exit 2
fi
if ! [[ "$workflow_id" =~ ^[0-9]+$ ]]; then
  echo "find-rehearsal: workflow id '$workflow_id' is not a number" >&2
  exit 2
fi

if ! runs=$(gh run list --workflow release.yml --event workflow_dispatch --branch main \
      --commit "$sha" --status success --limit 200 \
      --json databaseId,headSha,headBranch,event,conclusion,workflowDatabaseId,displayTitle,url); then
  echo "find-rehearsal: gh run list failed" >&2
  exit 2
fi

# Re-filter on every field: a server-side filter that stopped working must
# not let a wrong run through. The title is never matched on.
if ! urls=$(jq -er --argjson wid "$workflow_id" --arg sha "$sha" '
    if type != "array" then error("not an array") else . end
    | [ .[] | select(.workflowDatabaseId == $wid
                     and .event == "workflow_dispatch"
                     and .headBranch == "main"
                     and .headSha == $sha
                     and .conclusion == "success") | .url ]
    | if length == 0 then "" else .[0] end' <<<"$runs"); then
  echo "find-rehearsal: gh run list returned something other than a JSON array" >&2
  exit 2
fi

if [ -z "$urls" ]; then
  echo "find-rehearsal: no successful rehearsal of release.yml on main for $sha; dispatch release.yml on main with sha=$sha first" >&2
  exit 1
fi
echo "$urls"
