/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { join, posix } from "node:path";
import { createHash } from "node:crypto";

const symbols = process.env.OINK_SYMBOLS_DIR;

export default defineConfig({
  define: { __OINK_RELEASE_VERSION__: JSON.stringify(process.env.OINK_RELEASE_VERSION ?? JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")).version) },
  plugins: [react(), {
    name: "private-release-source-maps",
    writeBundle(options, bundle) {
      if (!symbols) return;
      mkdirSync(symbols, { recursive: true });
      const scripts: { file: string; sha256: string; source_map: string }[] = [];
      for (const [name, entry] of Object.entries(bundle)) {
        if (entry.type !== "chunk" || !entry.map) continue;
        const map = JSON.parse(readFileSync(join(options.dir!, name + ".map"), "utf8"));
        const code = readFileSync(join(options.dir!, name));
        map.sources = map.sources.map((source: string) => {
          if (/^(?:[a-z]+:|\/|[A-Za-z]:)/.test(source)) throw new Error("Absolute source-map path");
          return "oink:///" + posix.normalize(posix.join("apps/oink/dist", posix.dirname(name), source));
        });
        const base = name.replaceAll("/", "_");
        writeFileSync(join(symbols, base), code);
        writeFileSync(join(symbols, base + ".map"), JSON.stringify(map));
        scripts.push({ file: name, sha256: createHash("sha256").update(code).digest("hex"), source_map: base + ".map" });
      }
      // Copy final bytes after Vite's dependency-prefix rewrite, then remove
      // hidden maps before Tauri embeds the completed frontend directory.
      for (const name of Object.keys(bundle)) if (name.endsWith(".map")) unlinkSync(join(options.dir!, name));
      writeFileSync(join(symbols, "manifest.json"), JSON.stringify({ schema: 1,
        source_tree_sha256: process.env.OINK_SOURCE_TREE_SHA256,
        version: process.env.OINK_RELEASE_VERSION, build: process.env.OINK_RELEASE_BUILD,
        scripts }, null, 2) + "\n");
    },
  }],
  clearScreen: false,
  server: {
    port: 1440,
    strictPort: true,
    host: "127.0.0.1",
    watch: { ignored: ["**/src-tauri/**"] },
  },
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: { target: "es2022", outDir: "dist", emptyOutDir: true, sourcemap: symbols ? "hidden" : false },
  test: { include: ["src/**/*.test.{ts,tsx}"], environment: "node" },
});
