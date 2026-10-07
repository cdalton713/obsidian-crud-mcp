#!/bin/bash
set -e

echo "=== Obsidian CRUD MCP — Fly.io Setup ==="
echo ""
echo "Deploys the MCP server against the S3 bucket your Remotely Save plugin"
echo "syncs to (Cloudflare R2, AWS S3, Backblaze B2, MinIO, ...)."
echo ""
echo "Before you start (see README, Setup A):"
echo "  1. An R2 bucket and an R2 API token (Object Read & Write)"
echo "  2. Remotely Save syncing your vault to that bucket"
echo "  3. Optional: a Cloudflare AI Search instance on the bucket and an API"
echo "     token with AI Search Edit + Run, for semantic search"
echo ""

# Check flyctl is installed
if ! command -v fly &> /dev/null; then
    echo "flyctl not found. Install it:"
    echo "  curl -L https://fly.io/install.sh | sh"
    echo "  export PATH=\"\$HOME/.fly/bin:\$PATH\""
    exit 1
fi

# Check logged in
if ! fly auth whoami &> /dev/null; then
    echo "Not logged in to Fly.io. Run:"
    echo "  fly auth login"
    exit 1
fi

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DEPLOY_DIR="$REPO_DIR/deploy/mcp-only"

# Ask for vault name
printf "Obsidian vault name (for deep links) [MyVault]: "
read -r VAULT_NAME
VAULT_NAME="${VAULT_NAME:-MyVault}"

# MCP auth token
MCP_AUTH_TOKEN=$(openssl rand -hex 16)

# S3 connection: the same values as in Remotely Save's settings
echo ""
echo "Enter the S3 settings from Remotely Save:"
printf "Endpoint (e.g. https://<account-id>.r2.cloudflarestorage.com): "
read -r S3_ENDPOINT
printf "Region [auto]: "
read -r S3_REGION
S3_REGION="${S3_REGION:-auto}"
printf "Bucket name: "
read -r S3_BUCKET
printf "Remote prefix (leave blank if none): "
read -r S3_PREFIX
printf "Access Key ID: "
read -r S3_ACCESS_KEY_ID
printf "Secret Access Key: "
read -rs S3_SECRET_ACCESS_KEY
echo

# Optional: Cloudflare AI Search for the semantic_search tool
echo ""
echo "Semantic search (optional): Cloudflare AI Search settings."
printf "Cloudflare account ID (leave blank to skip semantic search): "
read -r CF_ACCOUNT_ID
if [ -n "$CF_ACCOUNT_ID" ]; then
    printf "AI Search instance name [obsidian-vault]: "
    read -r CF_AI_SEARCH_INSTANCE
    CF_AI_SEARCH_INSTANCE="${CF_AI_SEARCH_INSTANCE:-obsidian-vault}"
    printf "API token with AI Search Edit + Run: "
    read -rs CF_AI_SEARCH_TOKEN
    echo
fi

echo ""
echo "Deploying..."
echo ""

# Launch app
cd "$DEPLOY_DIR"
fly launch --no-deploy --copy-config

# Get the app name
APP_NAME=$(grep "^app " fly.toml | sed "s/app = ['\"]*//" | sed "s/['\"]//g" | tr -d ' ')

# Allocate shared IPv4 (free) and IPv6
fly ips allocate-v4 --shared 2>/dev/null || true
fly ips allocate-v6 2>/dev/null || true

# Set secrets (each value quoted to handle spaces in vault names)
fly secrets set \
    "MCP_AUTH_TOKEN=$MCP_AUTH_TOKEN" \
    "VAULT_NAME=$VAULT_NAME" \
    "BASE_URL=https://${APP_NAME}.fly.dev" \
    "S3_ENDPOINT=$S3_ENDPOINT" \
    "S3_REGION=$S3_REGION" \
    "S3_BUCKET=$S3_BUCKET" \
    "S3_ACCESS_KEY_ID=$S3_ACCESS_KEY_ID" \
    "S3_SECRET_ACCESS_KEY=$S3_SECRET_ACCESS_KEY" \
    ${S3_PREFIX:+"S3_PREFIX=$S3_PREFIX"} \
    ${CF_ACCOUNT_ID:+"CF_ACCOUNT_ID=$CF_ACCOUNT_ID"} \
    ${CF_ACCOUNT_ID:+"CF_AI_SEARCH_INSTANCE=$CF_AI_SEARCH_INSTANCE"} \
    ${CF_ACCOUNT_ID:+"CF_AI_SEARCH_TOKEN=$CF_AI_SEARCH_TOKEN"}

# Create volume for the vault mirror, search index and OAuth tokens
REGION=$(grep "primary_region" fly.toml | sed "s/.*= *['\"]*//" | sed "s/['\"].*//")
fly volumes create mcp_data --size 1 --region "$REGION" -y || true

# Build this checkout on Fly's remote builder and deploy (no local Docker needed)
cd "$REPO_DIR"
fly deploy . --config "$DEPLOY_DIR/fly.toml" --dockerfile Dockerfile

# Ensure single machine (auth state is in-memory, multiple machines break OAuth)
fly scale count 1 -y 2>/dev/null || true

echo ""
echo "=== Setup Complete ==="
echo ""
echo "Save these credentials — they won't be shown again."
echo ""
echo "MCP endpoint:     https://${APP_NAME}.fly.dev/mcp"
echo "MCP password:     $MCP_AUTH_TOKEN"
echo ""
echo "Connect Claude: Settings -> Connectors -> Add custom connector, using the endpoint above."
echo "Redeploy after a git pull: fly deploy . --config deploy/mcp-only/fly.toml --dockerfile Dockerfile"
