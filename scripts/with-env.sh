#!/usr/bin/env bash
# Sources .env (KEY=value, one per line) and execs the given command with those
# variables exported. Keeps secrets out of shell history and transcripts.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ! -f .env ]]; then
  echo "no .env file — create one with TELOXIDE_TOKEN=... (see README)" >&2
  exit 1
fi
set -a
# shellcheck disable=SC1091
source .env
set +a
exec "$@"
