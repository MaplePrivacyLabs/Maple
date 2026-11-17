#!/usr/bin/env bash
set -euo pipefail

expected='nix (Determinate Nix 3.24.2) 2.35.3'
actual="$(nix --version)"

if [[ "$actual" != "$expected" ]]; then
  printf 'error: expected %s, got %s\n' "$expected" "$actual" >&2
  exit 1
fi

printf '%s\n' "$actual"
