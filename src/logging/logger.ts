import type { Logger } from "fastmcp";
import pino from "pino";
import { describeLogMessage } from "./redact.js";

export const logger = pino(
    {
        level: process.env.LOG_LEVEL ?? "info",
        hooks: {
            logMethod(args, method) {
                const verbose = this.isLevelEnabled("debug");
                method.call(this, args.map((arg) => describeLogMessage(arg, verbose)).join(" "));
            },
        },
    },
    // Keep logs out of MCP stdout and flush before immediate process exits.
    pino.destination({ dest: 2, sync: true }),
);

// FastMCP accepts arbitrary arguments and requires a console-style log method.
// The logMethod hook above already redacts and joins every argument.
function logMcp(level: "debug" | "info" | "warn" | "error", args: unknown[]): void {
    const log: (...messages: unknown[]) => void = logger[level].bind(logger);
    log(...args);
}

export const mcpLogger: Logger = {
    debug: (...args) => logMcp("debug", args),
    info: (...args) => logMcp("info", args),
    log: (...args) => logMcp("info", args),
    warn: (...args) => logMcp("warn", args),
    error: (...args) => logMcp("error", args),
};
