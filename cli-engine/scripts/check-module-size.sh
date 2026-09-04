#!/usr/bin/env bash
# Fails if any hand-written .rs file exceeds the line-count limit — see
# AGENTS.md's "Code File Structure" section for the file-layout convention
# this enforces.
#
# Scope: cli-engine/src and cli-engine-macros/src — every workspace member's
# hand-written source. Deliberately excludes target/ (build output; nothing
# under it is committed source).
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
limit=1000
violations=0

while IFS= read -r -d '' file; do
  lines=$(wc -l < "$file")
  if [ "$lines" -gt "$limit" ]; then
    echo "  $file: $lines lines (limit $limit)"
    violations=$((violations + 1))
  fi
done < <(find "$repo_root/cli-engine/src" "$repo_root/cli-engine-macros/src" \
  -name '*.rs' -print0 2>/dev/null)

if [ "$violations" -gt 0 ]; then
  echo "ERROR: $violations file(s) over the ${limit}-line limit (shown above)."
  echo "Split by concern into a directory module — see AGENTS.md."
  exit 1
fi

echo "==> All .rs files are within the ${limit}-line limit"
