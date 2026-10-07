import { describe, it } from "vitest";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";

function runLogger(level: string) {
    return spawnSync(
        process.execPath,
        [
            "--import",
            "tsx",
            "--input-type=module",
            "-e",
            `
        import { logger } from './src/logging/logger.ts';
        logger.debug('debug message');
        logger.info('ready');
        logger.error('save failed:', new Error('http://user:secret@host/db'));
    `,
        ],
        { encoding: "utf8", env: { ...process.env, LOG_LEVEL: level } },
    );
}

describe("logger", () => {
    it("includes debug messages only at the debug level", () => {
        const result = runLogger("debug");
        assert.equal(result.status, 0, result.stderr);
        const records = result.stderr
            .trim()
            .split("\n")
            .map((line) => JSON.parse(line));
        assert.deepEqual(
            records.map((record) => record.level),
            [20, 30, 50],
        );
        assert.ok(records[2].msg.includes("at "));
        assert.ok(!result.stderr.includes("secret"));
    });
    it("writes JSON to stderr and redacts credentials in errors", () => {
        const result = runLogger("info");
        assert.equal(result.status, 0, result.stderr);
        assert.equal(result.stdout, "");
        const records = result.stderr
            .trim()
            .split("\n")
            .map((line) => JSON.parse(line));
        assert.deepEqual(
            records.map((record) => record.level),
            [30, 50],
        );
        assert.equal(records[0].msg, "ready");
        assert.equal(records[1].msg, "save failed: Error: http://***@host/db");
        assert.ok(!result.stderr.includes("secret"));
    });
});

it("routes FastMCP log calls through Pino with credential redaction", () => {
    const result = spawnSync(
        process.execPath,
        [
            "--import",
            "tsx",
            "--input-type=module",
            "-e",
            `
        import { mcpLogger } from './src/logging/logger.ts';
        mcpLogger.log('MCP ready');
        mcpLogger.warn('request failed', { url: 'http://user:secret@host/db' });
    `,
        ],
        { encoding: "utf8", env: { ...process.env, LOG_LEVEL: "info" } },
    );
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, "");
    const records = result.stderr
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line));
    assert.deepEqual(
        records.map((record) => record.level),
        [30, 40],
    );
    assert.equal(records[0].msg, "MCP ready");
    assert.ok(records[1].msg.includes("http://***@host/db"));
    assert.ok(!result.stderr.includes("secret"));
});
