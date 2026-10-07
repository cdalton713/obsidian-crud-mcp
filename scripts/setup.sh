#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Read the bootstrap version without requiring Node, Python, or proto.
proto_version="$(sed -nE 's/^proto[[:space:]]*=[[:space:]]*"([0-9]+\.[0-9]+\.[0-9]+)"[[:space:]]*$/\1/p' .prototools)"
if [[ -z "$proto_version" ]]; then
    echo "Error: .prototools must pin an exact proto version." >&2
    exit 1
fi

if [[ -n "${PROTO_HOME:-}" ]]; then
    proto_root="$PROTO_HOME"
elif [[ -n "${XDG_DATA_HOME:-}" ]]; then
    proto_root="$XDG_DATA_HOME/proto"
else
    proto_root="$HOME/.proto"
fi
export PROTO_HOME="$proto_root"
proto_bin="$proto_root/bin/proto"

if [[ ! -x "$proto_bin" ]] || [[ "$("$proto_bin" --version)" != "proto $proto_version" ]]; then
    echo "Installing proto $proto_version..."
    installer="$(mktemp)"
    trap 'rm -f -- "$installer"' EXIT
    curl --fail --silent --show-error --location https://moonrepo.dev/install/proto.sh --output "$installer"
    bash "$installer" "$proto_version" --yes
fi

export PATH="$proto_root/shims:$proto_root/bin:$PATH"
"$proto_bin" install --config-mode local
"$proto_bin" exec --config-mode local node pnpm -- pnpm install --frozen-lockfile
"$proto_bin" exec --config-mode local node pnpm -- pnpm run build

echo "Setup complete. Open a new terminal to use the pinned node and pnpm commands."
