#!/usr/bin/env node
/**
 * Read-only audit of extracted JS hooks versus compiled IR.
 * Does not rewrite closures. Prints a risk table and fails only when an
 * IR hook id has no file on disk.
 */
import { readdir, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { hookFileName } from "./compile-spec-ir.mjs";

const repoDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const irDir = process.env.EC_SPECS_IR || join(repoDir, "bundle", "specs-ir");
const hooksDir = join(irDir, "hooks");

const HOOK_FIELDS = ["jsPostProcess", "jsCustom", "jsScript", "jsGenerateSpec"];
const RISK = [/fig\./, /require\s*\(/, /Intl\./, /\bprocess\b/, /\bwindow\b/];

async function walkJson(dir, acc = []) {
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch {
    return acc;
  }
  for (const entry of entries) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "hooks") continue;
      await walkJson(full, acc);
    } else if (entry.isFile() && entry.name.endsWith(".json") && entry.name !== "index.json") {
      acc.push(full);
    }
  }
  return acc;
}

function collectHookIds(value, ids) {
  if (Array.isArray(value)) {
    for (const item of value) collectHookIds(item, ids);
    return;
  }
  if (!value || typeof value !== "object") return;
  for (const field of HOOK_FIELDS) {
    if (typeof value[field] === "string" && value[field]) ids.add(value[field]);
  }
  for (const child of Object.values(value)) collectHookIds(child, ids);
}

const files = await walkJson(irDir);
const ids = new Set();
for (const file of files) {
  collectHookIds(JSON.parse(await readFile(file, "utf8")), ids);
}

let hookFiles = [];
try {
  hookFiles = (await readdir(hooksDir)).filter((name) => name.endsWith(".js"));
} catch {
  hookFiles = [];
}

const missing = [];
for (const id of ids) {
  const name = hookFileName(id);
  if (!hookFiles.includes(name)) missing.push(id);
}

const risky = [];
for (const name of hookFiles) {
  const source = await readFile(join(hooksDir, name), "utf8");
  const hits = RISK.filter((pattern) => pattern.test(source)).map((pattern) => String(pattern));
  if (hits.length) risky.push({ file: name, hits });
}

process.stdout.write(
  JSON.stringify(
    {
      irSpecs: files.length,
      hookIds: ids.size,
      hookFiles: hookFiles.length,
      missingFiles: missing,
      riskyHooks: risky.length,
      risky: risky.slice(0, 40),
    },
    null,
    2,
  ) + "\n",
);

if (missing.length) {
  process.stderr.write(`error: ${missing.length} IR hook ids have no file in ${hooksDir}\n`);
  process.exit(1);
}
if (hookFiles.length === 0) {
  process.stderr.write(`error: no hook files in ${hooksDir}\n`);
  process.exit(1);
}
