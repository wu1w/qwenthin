#!/usr/bin/env bash
# Optional: build q38 if needed, then install the dsh profile + plugin.
# Product shell is `q38 web`. This path is for people who already run dsh.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if ! command -v q38 >/dev/null 2>&1; then
  echo "building q38…"
  cargo build -p q38-cli --release
  export PATH="$root/target/release:$PATH"
  export Q38_BIN="$root/target/release/q38"
fi

export Q38_DSH_PLUGIN="${Q38_DSH_PLUGIN:-$root/plugins/dsh-plugin-q38}"
exec q38 dsh-install "$@"
