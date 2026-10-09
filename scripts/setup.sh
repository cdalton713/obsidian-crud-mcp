#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# rust-toolchain.toml pins the Rust version; rustup installs it on first use.
if ! command -v rustup >/dev/null 2>&1; then
    echo "Installing rustup..."
    installer="$(mktemp)"
    trap 'rm -f -- "$installer"' EXIT
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location https://sh.rustup.rs --output "$installer"
    sh "$installer" -y --default-toolchain none
    # shellcheck source=/dev/null
    . "${CARGO_HOME:-$HOME/.cargo}/env"
fi

rustup show active-toolchain >/dev/null
cargo build --locked
git config core.hooksPath .githooks

echo "Setup complete. Open a new terminal if cargo was just installed."
