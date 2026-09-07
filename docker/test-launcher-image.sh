#!/usr/bin/env bash
# Host-side Docker certification; no shell or test utility is added to the image.
set -euo pipefail
image=${1:?usage: test-launcher-image.sh IMAGE APP [FIXED_ARGS...]}
shift
app=${1:?missing application path}
shift
work=$(mktemp -d)
expected=$(jq -cn --args '$ARGS.positional' /ores-launcher "$app" "$@")
docker image inspect "$image" | jq -e --argjson expected "$expected" '
  .[0].Config | .Entrypoint == $expected and
  (.Cmd // []) == [] and .User == "65532:65532"
'
flags=(--read-only --network none --cap-drop ALL --security-opt no-new-privileges)
# Verify actual loader/launcher execution, not just static Dockerfile text.
set +e
docker run --rm "${flags[@]}" --entrypoint /ores-launcher "$image" >"$work/empty.out" 2>"$work/empty.err"
status=$?
set -e
test "$status" -eq 64
jq -e '.schema == "next-loggers/v1" and .fields["event.name"] == "process.exec.invalid_command"' "$work/empty.err"
set +e
docker run --rm "${flags[@]}" --entrypoint /ores-launcher "$image" /__ores_missing_command__ 'two words' '' '*' >"$work/missing.out" 2>"$work/missing.err"
status=$?
set -e
test "$status" -eq 127
jq -s -e '
  [.[] | select(.schema == "next-loggers/v1" and .fields["event.name"] == "process.exec.attempt")] |
  length == 1 and .[0].fields["process.pid"] == 1 and
  .[0].fields["process.command_args"] == ["/__ores_missing_command__", "two words", "", "*"]
' "$work/missing.err"
test ! -s "$work/empty.out"
test ! -s "$work/missing.out"
for shell in /bin/sh /bin/bash; do
  if docker run --rm "${flags[@]}" --entrypoint "$shell" "$image" -c ':' >"$work/shell.out" 2>"$work/shell.err"; then
    printf 'unexpected runtime shell: %s\n' "$shell" >&2
    exit 1
  fi
  grep -Eq 'no such file|executable file not found' "$work/shell.err"
done
printf '%s\n' 'PASS: image metadata, nonroot, shell-free launcher, canonical stderr logs, literal argv logging, exits 64/127'
printf '%s\n' 'This does not certify application startup, database access, kubelet probes, or production deployment.'
