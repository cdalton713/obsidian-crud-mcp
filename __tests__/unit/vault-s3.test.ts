import { describe, it, beforeEach, afterEach } from "vitest";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile, readFile, stat, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
    CopyObjectCommand,
    DeleteObjectCommand,
    GetObjectCommand,
    ListObjectsV2Command,
    PutObjectCommand,
    S3Client,
} from "@aws-sdk/client-s3";
import { mockClient } from "aws-sdk-client-mock";
import {
    S3Mirror,
    normalizePrefix,
    parseRemoteMtime,
    planMirror,
    remoteMetadata,
    RemoteChangedError,
} from "../../src/vault/s3-mirror.js";
import { S3Vault } from "../../src/vault/vault-s3.js";

interface StoredObject {
    body: string;
    etag: string;
    metadata: Record<string, string>;
    lastModified: Date;
}

/** An in-memory bucket behind a mocked S3Client, with conditional-write support. */
function fakeBucket() {
    const objects = new Map<string, StoredObject>();
    let version = 0;
    const nextEtag = () => `"etag-${++version}"`;
    const precondition = () =>
        Object.assign(new Error("PreconditionFailed"), { $metadata: { httpStatusCode: 412 } });

    const client = new S3Client({ region: "auto" });
    const mock = mockClient(client);
    /** While set, listings snapshot the bucket, then wait here before answering. */
    let listGate: Promise<void> | null = null;
    mock.on(ListObjectsV2Command).callsFake(async (input) => {
        const snapshot = [...objects.entries()]
            .filter(([key]) => key.startsWith(input.Prefix ?? ""))
            .sort(([a], [b]) => (a < b ? -1 : 1));
        if (listGate) await listGate;
        // Two keys per page so pagination is exercised.
        const start = input.ContinuationToken ? Number(input.ContinuationToken) : 0;
        const page = snapshot.slice(start, start + 2);
        const more = start + 2 < snapshot.length;
        return {
            Contents: page.map(([key, object]) => ({
                Key: key,
                ETag: object.etag,
                LastModified: object.lastModified,
            })),
            IsTruncated: more,
            NextContinuationToken: more ? String(start + 2) : undefined,
        };
    });
    mock.on(GetObjectCommand).callsFake((input) => {
        const object = objects.get(input.Key);
        if (!object)
            throw Object.assign(new Error("NoSuchKey"), { $metadata: { httpStatusCode: 404 } });
        return {
            // Only the method the mirror calls; the SDK stream type is not needed here.
            Body: { transformToByteArray: async () => new Uint8Array(Buffer.from(object.body)) },
            ETag: object.etag,
            Metadata: Object.fromEntries(
                Object.entries(object.metadata).map(([k, v]) => [k.toLowerCase(), v]),
            ),
        };
    });
    mock.on(PutObjectCommand).callsFake((input) => {
        const existing = objects.get(input.Key);
        if (input.IfNoneMatch === "*" && existing) throw precondition();
        if (input.IfMatch && existing?.etag !== input.IfMatch) throw precondition();
        const etag = nextEtag();
        objects.set(input.Key, {
            body: String(input.Body),
            etag,
            metadata: input.Metadata ?? {},
            lastModified: new Date(),
        });
        return { ETag: etag };
    });
    mock.on(CopyObjectCommand).callsFake((input) => {
        const sourceKey = decodeURIComponent(input.CopySource.slice(input.Bucket.length + 1));
        const source = objects.get(sourceKey)!;
        const etag = nextEtag();
        objects.set(input.Key, { ...source, etag });
        return { CopyObjectResult: { ETag: etag } };
    });
    mock.on(DeleteObjectCommand).callsFake((input) => {
        objects.delete(input.Key);
        return {};
    });

    /** Simulate Remotely Save uploading a note from a device. */
    const deviceUpload = (key: string, body: string, mtimeMs: number) =>
        objects.set(key, {
            body,
            etag: nextEtag(),
            metadata: remoteMetadata(mtimeMs),
            lastModified: new Date(),
        });

    return {
        client,
        mock,
        objects,
        deviceUpload,
        holdListings: (gate: Promise<void> | null) => void (listGate = gate),
    };
}

describe("s3 mirror helpers", () => {
    it("reads Remotely Save mtime in seconds, legacy milliseconds, and falls back when absent", () => {
        assert.equal(parseRemoteMtime({ mtime: "1700000000.5" }), 1_700_000_000_000);
        assert.equal(parseRemoteMtime({ MTime: "1700000000123" }), 1_700_000_000_123);
        assert.equal(parseRemoteMtime({ mtime: "0" }), undefined);
        assert.equal(parseRemoteMtime({}), undefined);
        assert.equal(parseRemoteMtime(undefined), undefined);
    });

    it("writes metadata in the seconds format the plugin reads", () => {
        assert.deepEqual(remoteMetadata(1_700_000_000_500), {
            MTime: "1700000000.5",
            CTime: "1700000000.5",
        });
    });

    it("normalizes prefixes to empty or a trailing slash", () => {
        assert.equal(normalizePrefix(""), "");
        assert.equal(normalizePrefix("/vault/"), "vault/");
        assert.equal(normalizePrefix("a/b"), "a/b/");
    });

    it("plans downloads for new, changed, and locally missing notes, and removes only tracked notes", () => {
        const manifest = {
            version: 1 as const,
            files: {
                "same.md": { etag: '"1"', mtime: 1 },
                "changed.md": { etag: '"1"', mtime: 1 },
                "missing-locally.md": { etag: '"1"', mtime: 1 },
                "deleted-remotely.md": { etag: '"1"', mtime: 1 },
            },
        };
        const remote = [
            { path: "same.md", etag: '"1"', lastModified: 1 },
            { path: "changed.md", etag: '"2"', lastModified: 1 },
            { path: "missing-locally.md", etag: '"1"', lastModified: 1 },
            { path: "new.md", etag: '"1"', lastModified: 1 },
        ];
        const plan = planMirror(remote, manifest, (path) => path !== "missing-locally.md");
        assert.deepEqual(plan.download.map((note) => note.path).sort(), [
            "changed.md",
            "missing-locally.md",
            "new.md",
        ]);
        assert.deepEqual(plan.remove, ["deleted-remotely.md"]);
    });
});

describe("S3Vault", () => {
    let dir: string;
    let vaultPath: string;
    let bucket: ReturnType<typeof fakeBucket>;
    let vault: S3Vault;

    const open = (prefix = "", writeFolders: string[] | null = null) => {
        const options = {
            vaultPath,
            manifestPath: join(dir, "data", "s3-manifest.json"),
            pollSeconds: 3600,
            writeFolders,
            region: "auto",
            bucket: "vault",
            prefix,
        };
        const mirror = new S3Mirror(vaultPath, options.manifestPath, options, bucket.client);
        return new S3Vault(options, mirror);
    };

    beforeEach(async () => {
        dir = await mkdtemp(join(tmpdir(), "vault-s3-"));
        vaultPath = join(dir, "vault");
        await mkdir(vaultPath);
        bucket = fakeBucket();
    });

    afterEach(async () => {
        await vault?.close();
        bucket.mock.restore();
        await rm(dir, { recursive: true, force: true });
    });

    it("mirrors notes on init with the device's mtime, skipping non-notes", async () => {
        const mtime = 1_700_000_000_000;
        bucket.deviceUpload("Daily/2026-10-06.md", "# today", mtime);
        bucket.deviceUpload("Inbox.md", "inbox", mtime);
        bucket.deviceUpload("image.png", "binary", mtime);
        bucket.deviceUpload(".obsidian/app.md", "config", mtime);
        bucket.deviceUpload("_remotely-save-metadata-on-remote.json", "{}", mtime);
        bucket.deviceUpload("_debug_remotely_save/log.md", "debug", mtime);
        vault = open();
        await vault.init();

        assert.deepEqual(await vault.listNotes(), ["Daily/2026-10-06.md", "Inbox.md"]);
        assert.equal(await vault.readNote("Daily/2026-10-06.md"), "# today");
        assert.equal((await stat(join(vaultPath, "Inbox.md"))).mtimeMs, mtime);
        await assert.rejects(stat(join(vaultPath, "image.png")));
    });

    it("strips the configured prefix and ignores keys outside it", async () => {
        bucket.deviceUpload("vault/Note.md", "inside", 1_700_000_000_000);
        bucket.deviceUpload("other/Note.md", "outside", 1_700_000_000_000);
        vault = open("vault");
        await vault.init();
        assert.deepEqual(await vault.listNotes(), ["Note.md"]);
        assert.equal(await vault.readNote("Note.md"), "inside");
    });

    it("picks up remote edits and deletions on the next poll, leaving untracked local files alone", async () => {
        bucket.deviceUpload("a.md", "v1", 1_700_000_000_000);
        bucket.deviceUpload("b.md", "keep", 1_700_000_000_000);
        vault = open();
        await vault.init();
        await writeFile(join(vaultPath, "local-only.md"), "untracked");

        bucket.deviceUpload("a.md", "v2", 1_700_000_100_000);
        bucket.objects.delete("b.md");
        assert.equal(await vault.poll(), 2);

        assert.equal(await vault.readNote("a.md"), "v2");
        assert.equal(await vault.readNote("b.md"), null);
        assert.equal(await vault.readNote("local-only.md"), "untracked");
        assert.equal(await vault.poll(), 0);
    });

    it("reports what a poll downloaded or removed to subscribers, but not its own writes", async () => {
        bucket.deviceUpload("a.md", "v1", 1_700_000_000_000);
        bucket.deviceUpload("b.md", "bye", 1_700_000_000_000);
        vault = open();
        const events: unknown[] = [];
        const unsubscribe = vault.subscribe({
            updated: (path, content, mtime) => events.push(["updated", path, content, mtime]),
            removed: (path) => events.push(["removed", path]),
        });
        await vault.init();
        assert.deepEqual(events.sort(), [
            ["updated", "a.md", "v1", 1_700_000_000_000],
            ["updated", "b.md", "bye", 1_700_000_000_000],
        ]);
        // The reported mtime is the mirrored file's, so an mtime-based rescan skips it.
        assert.equal((await stat(join(vaultPath, "a.md"))).mtimeMs, 1_700_000_000_000);

        events.length = 0;
        await vault.writeNote("c.md", "mine");
        assert.deepEqual(events, []);

        bucket.deviceUpload("a.md", "v2", 1_700_000_100_000);
        bucket.objects.delete("b.md");
        await vault.poll();
        assert.deepEqual(events, [
            ["updated", "a.md", "v2", 1_700_000_100_000],
            ["removed", "b.md"],
        ]);

        unsubscribe();
        bucket.deviceUpload("a.md", "v3", 1_700_000_200_000);
        await vault.poll();
        assert.equal(events.length, 2);
    });

    it("completes writes and deletes while a poll waits on the bucket, and the poll does not undo them", async () => {
        bucket.deviceUpload("a.md", "v1", 1_700_000_000_000);
        bucket.deviceUpload("gone.md", "bye", 1_700_000_000_000);
        vault = open();
        await vault.init();

        let release!: () => void;
        bucket.holdListings(new Promise<void>((resolve) => (release = resolve)));
        const polling = vault.poll(); // its listing shows a.md=v1 and gone.md, no new.md
        // None of these may wait for the poll (the test would time out).
        await vault.writeNote("new.md", "created meanwhile");
        await vault.writeNote("a.md", "edited meanwhile");
        assert.equal(await vault.deleteNote("gone.md"), true);
        assert.equal(await vault.readNote("a.md"), "edited meanwhile");
        assert.equal(bucket.objects.get("a.md")?.body, "edited meanwhile");

        release();
        await polling;
        // The stale listing neither removed new.md, re-downloaded a.md, nor resurrected gone.md.
        assert.equal(await vault.readNote("new.md"), "created meanwhile");
        assert.equal(await vault.readNote("a.md"), "edited meanwhile");
        assert.equal(await vault.readNote("gone.md"), null);
        assert.deepEqual(
            bucket.mock.commandCalls(GetObjectCommand).map((c) => c.args[0].input.Key),
            ["a.md", "gone.md"],
            "only the initial mirror downloaded anything",
        );
        // The manifest agrees with the bucket, so the next poll is a no-op.
        bucket.holdListings(null);
        assert.equal(await vault.poll(), 0);
    });

    it("does not re-download unchanged notes after a restart", async () => {
        bucket.deviceUpload("a.md", "v1", 1_700_000_000_000);
        vault = open();
        await vault.init();
        await vault.close();

        vault = open();
        await vault.init();
        assert.equal(bucket.mock.commandCalls(GetObjectCommand).length, 1);
    });

    it("uploads writes with Remotely Save metadata before writing locally", async () => {
        vault = open();
        await vault.init();
        assert.equal(await vault.writeNote("Notes/new.md", "hello"), true);

        const stored = bucket.objects.get("Notes/new.md")!;
        assert.equal(stored.body, "hello");
        assert.match(stored.metadata.MTime, /^\d+(\.\d+)?$/);
        assert.equal(await readFile(join(vaultPath, "Notes/new.md"), "utf8"), "hello");
        // The note is tracked, so the next poll neither downloads nor deletes it.
        assert.equal(await vault.poll(), 0);
    });

    it("leaves the local copy untouched when the upload fails", async () => {
        bucket.deviceUpload("a.md", "v1", 1_700_000_000_000);
        vault = open();
        await vault.init();
        bucket.mock.on(PutObjectCommand).rejects(new Error("network down"));
        await assert.rejects(vault.writeNote("a.md", "v2"), /network down/);
        assert.equal(await vault.readNote("a.md"), "v1");
    });

    it("refuses to overwrite a note that changed in the bucket since the last poll", async () => {
        bucket.deviceUpload("a.md", "v1", 1_700_000_000_000);
        vault = open();
        await vault.init();
        bucket.deviceUpload("a.md", "device edit", 1_700_000_100_000);

        await assert.rejects(vault.writeNote("a.md", "agent edit"), RemoteChangedError);
        assert.equal(bucket.objects.get("a.md")!.body, "device edit");

        await vault.poll();
        assert.equal(await vault.writeNote("a.md", "agent edit"), true);
        assert.equal(bucket.objects.get("a.md")!.body, "agent edit");
    });

    it("refuses to create a note that already exists remotely but has not been mirrored yet", async () => {
        vault = open();
        await vault.init();
        bucket.deviceUpload("race.md", "device", 1_700_000_000_000);
        await assert.rejects(vault.writeNote("race.md", "agent"), RemoteChangedError);
    });

    it("deletes remotely and locally", async () => {
        bucket.deviceUpload("a.md", "v1", 1_700_000_000_000);
        vault = open();
        await vault.init();
        assert.equal(await vault.deleteNote("a.md"), true);
        assert.equal(bucket.objects.has("a.md"), false);
        assert.equal(await vault.readNote("a.md"), null);
        assert.equal(await vault.deleteNote("a.md"), false);
    });

    it("moves with copy then delete, keeping the device mtime metadata", async () => {
        bucket.deviceUpload("a.md", "content", 1_700_000_000_000);
        vault = open();
        await vault.init();
        assert.equal(await vault.moveNote("a.md", "Folder/b.md"), true);

        assert.equal(bucket.objects.has("a.md"), false);
        assert.equal(bucket.objects.get("Folder/b.md")!.body, "content");
        assert.equal(bucket.objects.get("Folder/b.md")!.metadata.MTime, "1700000000");
        assert.deepEqual(await vault.listNotes(), ["Folder/b.md"]);
        assert.equal(await vault.poll(), 0);
    });

    it("refuses to move onto an existing note", async () => {
        bucket.deviceUpload("a.md", "a", 1_700_000_000_000);
        bucket.deviceUpload("b.md", "b", 1_700_000_000_000);
        vault = open();
        await vault.init();
        await assert.rejects(vault.moveNote("a.md", "b.md"), /Destination already exists/);
        assert.equal(bucket.objects.get("b.md")!.body, "b");
    });

    it("enforces write folders before touching the bucket", async () => {
        vault = open("", ["MCP"]);
        await vault.init();
        await assert.rejects(vault.writeNote("private.md", "x"), /Write access denied/);
        await assert.rejects(vault.writeNote(".obsidian/x.md", "x"), /Invalid note path/);
        assert.equal(bucket.mock.commandCalls(PutObjectCommand).length, 0);
    });
});
