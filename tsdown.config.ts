import { defineConfig } from "tsdown";
import { readFileSync } from "fs";

const pkg = JSON.parse(readFileSync("package.json", "utf-8"));

export default defineConfig({
    entry: ["src/main.ts"],
    format: ["esm"],
    fixedExtension: false,
    target: "node22",
    platform: "node",
    outDir: "dist",
    clean: true,
    sourcemap: true,
    banner: "#!/usr/bin/env node",
    deps: {
        onlyBundle: false,
        // Anything the output imports must be a declared runtime dependency.
        onlyImport: Object.keys(pkg.dependencies),
    },
    define: {
        "process.env.npm_package_version": JSON.stringify(pkg.version),
    },
});
