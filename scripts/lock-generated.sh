#!/usr/bin/env bash
# Make every generated/ tree read-only so accidental edits fail loudly.
# Git does not track the read-only bit, so run this after clone, after each
# generator run, and in CI. Exits non-zero if no generated/ tree exists.
set -euo pipefail
found=0
while IFS= read -r d; do
  found=1
  find "$d" -type f -exec chmod a-w {} +
  echo "locked: $d"
done < <(find . -type d -name generated \
           -not -path "./.git/*" -not -path "*/node_modules/*" \
           -not -path "*/target/*" -not -path "*/build/*")
[ "$found" -eq 1 ] || { echo "no generated/ directories found" >&2; exit 1; }
