# MCP server for Fly.io (S3 mode with a local mirror on the /data volume)
# For Fly.io GitHub deploy and CLI deploy
# Pulls the pre-built image from ghcr.io

ARG MCP_IMAGE=ghcr.io/cdalton713/obsidian-crud-mcp:latest
FROM ${MCP_IMAGE}

ENV DATA_DIR=/data

EXPOSE 8787

CMD ["node", "dist/main.js"]
