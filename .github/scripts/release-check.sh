#!/usr/bin/env bash
# Preflight only: no authentication, publication, or tag writes.
set -euo pipefail
v=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')
if [ "$REQUESTED_VERSION" != "$v" ]; then
  echo "Requested version does not match Cargo.toml ($v)" >&2
  exit 1
fi
echo "version=$v" >> "$GITHUB_OUTPUT"
code=$(curl --silent --show-error --connect-timeout 15 --max-time 60 -o /dev/null -w '%{http_code}' -A 'agent-workstation-release' "https://crates.io/api/v1/crates/agent-workstation/$v")
case "$code" in
  200) echo "publish=false" >> "$GITHUB_OUTPUT"; exit 0 ;;
  404) ;;
  *) echo "Unexpected crates.io response: $code" >&2; exit 1 ;;
esac
# A tag already claiming an unpublished version needs investigation, not overwrite.
if git ls-remote --exit-code --tags origin "refs/tags/v$v"; then
  echo "Release tag already exists: v$v" >&2
  exit 1
else
  status=$?
  [ "$status" -eq 2 ] || exit "$status"
fi
echo "publish=true" >> "$GITHUB_OUTPUT"
