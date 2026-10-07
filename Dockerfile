# MCP server image — used by CI to publish to ghcr.io

# Everything installed here is platform-independent JavaScript (no production
# dependency ships a native addon), so install and build once on the build host
# instead of under emulation for every target platform.
FROM --platform=$BUILDPLATFORM node:22-slim AS build

RUN corepack enable
WORKDIR /app

COPY package.json pnpm-lock.yaml ./
RUN --mount=type=cache,id=pnpm,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile

COPY tsconfig.json tsdown.config.ts ./
COPY src/ src/
RUN pnpm run build

# Production dependencies only, in a separate tree.
WORKDIR /prod
RUN cp /app/package.json /app/pnpm-lock.yaml ./
RUN --mount=type=cache,id=pnpm,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile --prod

# Runtime image without corepack's pnpm download or the pnpm store.
FROM node:22-slim

ENV NODE_ENV=production
WORKDIR /app

COPY package.json ./
COPY --from=build /prod/node_modules/ node_modules/
COPY --from=build /app/dist/ dist/

EXPOSE 8787

CMD ["node", "dist/main.js"]
