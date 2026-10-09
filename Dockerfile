# MCP server image, published to ghcr.io by CI.

FROM rust:1.99.0-slim-bookworm AS build

# aws-lc-sys (TLS for the S3 client) builds C code with cmake.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked \
    && cp target/release/obsidian-crud-mcp /usr/local/bin/obsidian-crud-mcp

# Runtime image: the binary plus CA certificates for S3 and Cloudflare.
FROM debian:bookworm-slim

LABEL io.modelcontextprotocol.server.name="io.github.cdalton713/obsidian-crud-mcp"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /usr/local/bin/obsidian-crud-mcp /usr/local/bin/obsidian-crud-mcp

EXPOSE 8787

CMD ["obsidian-crud-mcp"]
