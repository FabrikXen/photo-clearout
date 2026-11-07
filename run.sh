#!/usr/bin/env bash
# Starts photo-clearout. The Immich API key is read from 1Password, so it
# never sits in a file, in the code, or in your shell history.
set -euo pipefail
cd "$(dirname "$0")"

# The tool is built from source with Rust's cargo.
if ! command -v cargo >/dev/null; then
  echo "Rust is not installed. Install it from https://rustup.rs, then run ./run.sh again." >&2
  exit 1
fi
if [[ ! -x target/release/photo-clearout ]]; then
  echo "First run: building the tool. This takes a few minutes and only happens once…"
fi

# Build first, *before* the key is fetched: building runs code from the Rust
# libraries the tool uses, and that code must never be able to see the key.
# (If IMMICH_API_KEY is already set in your environment, it's hidden from the
# build too.)
env -u IMMICH_API_KEY cargo build --release --quiet
TOOL=target/release/photo-clearout

# Help needs no settings and no key.
if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  exec "$TOOL" --help
fi

# One setting from .env in this folder (not committed to git). Only that one
# line is read; the file is never run as a script. Surrounding quotes, as in
# NAME="value", are removed.
read_env() {
  local value=""
  if [[ -f .env ]]; then
    value="$(grep -E "^$1=" .env | tail -n 1 | cut -d= -f2- || true)"
  fi
  if [[ "$value" =~ ^\"(.*)\"$ || "$value" =~ ^\'(.*)\'$ ]]; then
    value="${BASH_REMATCH[1]}"
  fi
  printf '%s' "$value"
}

# Immich's address, unless IMMICH_URL is already set.
IMMICH_URL="${IMMICH_URL:-$(read_env IMMICH_URL)}"
if [[ -z "$IMMICH_URL" ]]; then
  echo "Immich's address is not set. Add this line to .env in this folder:" >&2
  echo "  IMMICH_URL=http://100.x.x.x:2283   (Immich's Tailscale address)" >&2
  exit 1
fi
export IMMICH_URL

# The API key comes from 1Password. .env only says *where* in 1Password it
# is (a "secret reference"). An IMMICH_API_KEY that's already set is used as is.
if [[ -z "${IMMICH_API_KEY:-}" ]]; then
  IMMICH_API_KEY_REF="${IMMICH_API_KEY_REF:-$(read_env IMMICH_API_KEY_REF)}"
  if [[ -z "$IMMICH_API_KEY_REF" ]]; then
    echo "Where the API key is in 1Password is not set. Add this line to .env:" >&2
    echo "  IMMICH_API_KEY_REF=op://<vault>/<item>/<field>" >&2
    exit 1
  fi
  if ! command -v op >/dev/null; then
    echo "The 1Password CLI is not installed: brew install 1password-cli" >&2
    exit 1
  fi
  if ! IMMICH_API_KEY="$(op read "$IMMICH_API_KEY_REF")"; then
    echo "Could not read the API key from 1Password ($IMMICH_API_KEY_REF)." >&2
    echo "Is 1Password unlocked, with Settings → Developer → Integrate with 1Password CLI on?" >&2
    exit 1
  fi
fi
export IMMICH_API_KEY

exec "$TOOL" "$@"
