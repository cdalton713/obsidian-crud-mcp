import { describe, it } from "vitest";
import assert from "node:assert/strict";
import {
    parseHostname,
    buildAllowedHosts,
    isHostAllowed,
    isOriginAllowed,
} from "../../src/auth/host-guard.js";

describe("parseHostname", () => {
    it("strips the port", () => {
        assert.equal(parseHostname("127.0.0.1:8787"), "127.0.0.1");
        assert.equal(parseHostname("localhost:8787"), "localhost");
    });
    it("handles bare hostnames", () => {
        assert.equal(parseHostname("attacker.example"), "attacker.example");
    });
    it("handles IPv6 with brackets and port", () => {
        assert.equal(parseHostname("[::1]:8787"), "::1");
    });
    it("lowercases", () => {
        assert.equal(parseHostname("Attacker.Example"), "attacker.example");
    });
    it("returns null for missing/empty", () => {
        assert.equal(parseHostname(undefined), null);
        assert.equal(parseHostname(""), null);
    });
    it("rejects userinfo/path smuggling that would normalize into the allowlist", () => {
        assert.equal(parseHostname("attacker.example@127.0.0.1"), null);
        assert.equal(parseHostname("127.0.0.1/../x"), null);
        assert.equal(parseHostname("attacker.example#@127.0.0.1"), null);
    });
});

describe("isHostAllowed — DNS-rebinding defense", () => {
    const allowed = buildAllowedHosts();

    it("allows localhost and loopback by default", () => {
        assert.ok(isHostAllowed("localhost:8787", allowed));
        assert.ok(isHostAllowed("127.0.0.1:8787", allowed));
        assert.ok(isHostAllowed("[::1]:8787", allowed));
    });

    it("rejects a rebound attacker hostname (the PoC case)", () => {
        assert.equal(isHostAllowed("attacker.example", allowed), false);
        assert.equal(isHostAllowed("attacker.example:8787", allowed), false);
    });

    it("rejects a LAN IP by default", () => {
        assert.equal(isHostAllowed("192.168.1.5:8787", allowed), false);
    });

    it("rejects missing Host", () => {
        assert.equal(isHostAllowed(undefined, allowed), false);
    });

    it("honors MCP_ALLOWED_HOSTS for legit LAN / private-network use", () => {
        const withExtra = buildAllowedHosts("192.168.1.5, mybox.local:8787");
        assert.ok(isHostAllowed("192.168.1.5:8787", withExtra));
        assert.ok(isHostAllowed("mybox.local", withExtra));
        // still rejects everything else
        assert.equal(isHostAllowed("attacker.example", withExtra), false);
    });
});

describe("isOriginAllowed — cross-origin (wildcard-CORS) defense", () => {
    const allowed = buildAllowedHosts();

    it("allows requests with no Origin (non-browser MCP clients)", () => {
        assert.ok(isOriginAllowed(undefined, allowed));
        assert.ok(isOriginAllowed("", allowed));
    });

    it("allows a local Origin (browser tool on localhost)", () => {
        assert.ok(isOriginAllowed("http://localhost:6274", allowed));
        assert.ok(isOriginAllowed("http://127.0.0.1:8787", allowed));
    });

    it("rejects a cross-origin browser Origin even with a loopback Host (the bypass)", () => {
        assert.equal(isOriginAllowed("http://attacker.example", allowed), false);
        assert.equal(isOriginAllowed("https://attacker.example", allowed), false);
    });

    it("rejects a null/opaque Origin", () => {
        assert.equal(isOriginAllowed("null", allowed), false);
    });
});
