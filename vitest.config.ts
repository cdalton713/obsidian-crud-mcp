import { defineConfig } from "vitest/config";

export default defineConfig({
    test: {
        reporters: ["dot"],
        projects: [
            {
                test: {
                    name: "unit",
                    include: ["__tests__/unit/**/*.test.ts"],
                    // pino writes straight to stderr, which would interleave with the dots.
                    env: { LOG_LEVEL: "silent" },
                },
            },
            {
                test: {
                    name: "e2e",
                    include: ["__tests__/e2e/**/*.test.ts"],
                    // Every suite spawns the built server on the same port.
                    fileParallelism: false,
                    testTimeout: 60_000,
                    hookTimeout: 60_000,
                },
            },
        ],
    },
});
