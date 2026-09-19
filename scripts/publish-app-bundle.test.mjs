import assert from "node:assert/strict";
import {
  access,
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { publish } from "./publish-app-bundle.mjs";

async function pathExists(path) {
  try {
    await access(path);
    return true;
  } catch (error) {
    if (error?.code === "ENOENT") return false;
    throw error;
  }
}

test("exclusive publication rollback confirms that final is absent", async (t) => {
  const temporaryRoot = process.platform === "darwin" ? "/private/tmp" : tmpdir();
  const publicationRoot = await mkdtemp(join(temporaryRoot, "easy-complete-publish-"));
  const finalPath = join(publicationRoot, "Fastab.app");
  const workRoot = await mkdtemp(
    join(publicationRoot, ".specs-inputs.publish-test-"),
  );
  const staging = join(workRoot, "Fastab.app");
  const swapHelper = join(workRoot, "fake-swap-helper");
  await mkdir(join(staging, "Contents"), { recursive: true });
  await writeFile(join(staging, "Contents", "marker"), "staging\n");
  await writeFile(
    swapHelper,
    `#!/usr/bin/env node
import { access, mkdir, rename, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

const [operation, source, destination] = process.argv.slice(2);
if (operation !== "exclusive") process.exit(2);
const sourceParent = dirname(source);
const destinationParent = dirname(destination);
const marker = join(
  sourceParent.includes(".specs-inputs.") ? sourceParent : destinationParent,
  ".first-exclusive-call",
);
let first = false;
try {
  await access(marker);
} catch (error) {
  if (error?.code !== "ENOENT") throw error;
  first = true;
  await writeFile(marker, "first\\n", { flag: "wx" });
}
await rename(source, destination);
if (first) {
  await rename(destination, join(dirname(source), "displaced-final"));
  await mkdir(destination);
}
`,
  );
  await chmod(swapHelper, 0o755);

  t.after(async () => {
    await rm(publicationRoot, { recursive: true, force: true });
    await rm(workRoot, { recursive: true, force: true });
  });

  await assert.rejects(
    publish(
      { staging, final: finalPath, swapHelper },
      {
        publicationRoot,
        expectedFinalPath: finalPath,
        lockPath: join(publicationRoot, ".app-publish.lock"),
      },
    ),
    /app publication changed directory identity/,
  );
  await assert.rejects(lstat(finalPath), { code: "ENOENT" });
  assert.equal(await pathExists(staging), true);
});
