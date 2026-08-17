import * as esbuild from "esbuild";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.dirname(fileURLToPath(import.meta.url));
const outfile = path.resolve(root, "../../../crates/ec_engine/js/engine.bundled.js");

await esbuild.build({
  absWorkingDir: path.resolve(root, ".."),
  entryPoints: ["src/index.ts"],
  bundle: true,
  format: "esm",
  platform: "neutral",
  outfile,
  logLevel: "silent",
  external: ["node:fs", "node:path", "node:url"],
});
