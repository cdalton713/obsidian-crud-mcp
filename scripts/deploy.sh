#!/usr/bin/env bash
# Build the image locally and deploy it to Fly.io with the repo-root fly.toml.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

docker build --platform linux/amd64 -t obsidian-crud-mcp:deploy .
DOCKER_HOST="${DOCKER_HOST:-$(docker context inspect --format '{{.Endpoints.docker.Host}}')}" \
    fly deploy deploy/mcp-only --config "$PWD/fly.toml" --local-only --build-arg MCP_IMAGE=obsidian-crud-mcp:deploy
