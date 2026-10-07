#!/usr/bin/env bash
# Makes a random API key for a self-hosted Worker and the GHOSTWRITIN_API_KEYS
# entry that admits it. The key is printed once, for you to keep; only its
# SHA-256 digest goes into the Worker's configuration.
#
#   tools/api-key.sh <account>
#   npx wrangler secret put GHOSTWRITIN_API_KEYS   # paste the entry (comma-join several)
set -euo pipefail

account="${1:?usage: tools/api-key.sh <account>}"
key="gw_$(openssl rand -hex 24)"
digest="$(printf %s "$key" | shasum -a 256 | cut -d' ' -f1)"

printf 'API key (keep it; it is not stored anywhere): %s\n' "$key" >&2
printf '%s=%s\n' "$account" "$digest"
