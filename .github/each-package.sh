#!/usr/bin/env bash
# Runs `cargo <subcommand> -p <package> <args...>` once per workspace
# member, and fails at the end if any run failed. The subcommand is every
# leading argument that doesn't start with `-` (`clippy`, `nextest run`).
#
# Plugins target different worlds (`plugin`, `streaming-plugin`) through
# `oxidhome-sdk` features, and cargo unifies features across the packages
# of one invocation, so `--workspace` would build every plugin with both
# worlds on. That fails: on wasm a component carries one world, and the
# SDK's export macros refuse the wrong one. One package per run keeps
# each plugin on its own world.
#
# Usage: .github/each-package.sh clippy --all-features --all-targets -- -D warnings
set -uo pipefail

sub=()
while [ "$#" -gt 0 ] && [[ "$1" != -* ]]; do
  sub+=("$1")
  shift
done
if [ "${#sub[@]}" -eq 0 ]; then
  echo "usage: $0 <cargo subcommand> [args...]" >&2
  exit 2
fi

packages=$(cargo metadata --no-deps --format-version 1 --locked |
  jq -r '.packages[].name' | sort) || exit 1

status=0
for package in $packages; do
  echo "::group::cargo ${sub[*]} -p $package" >&2
  cargo "${sub[@]}" -p "$package" "$@" || status=1
  echo "::endgroup::" >&2
done
exit "$status"
