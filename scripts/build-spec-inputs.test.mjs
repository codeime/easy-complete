import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import test from "node:test";

import {
  copyTreeContents,
  removeOwnedDirectory,
  parseArguments,
  verifySnapshotForAssembly,
  writeSnapshotManifest,
} from "./build-spec-inputs.mjs";
import { createPairMarker, writePairMarker } from "./spec-pair.mjs";

const helperPath = fileURLToPath(new URL("./build-spec-inputs.mjs", import.meta.url));
const helperUrl = pathToFileURL(helperPath).href;
const repoDir = resolve(fileURLToPath(new URL("..", import.meta.url)));

const delay = (milliseconds) =>
  new Promise((resolveDelay) => setTimeout(resolveDelay, milliseconds));

async function waitFor(condition, description, { timeoutMs = 5_000, intervalMs = 20 } = {}) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = condition();
    if (value) return value;
    await delay(Math.min(intervalMs, Math.max(1, deadline - Date.now())));
  }
  throw new Error(`timed out waiting for ${description}`);
}

function processExists(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if (error?.code === "ESRCH") return false;
    if (error?.code === "EPERM") return true;
    throw error;
  }
}

async function waitForProcessExit(pid, description, timeoutMs = 5_000) {
  await waitFor(
    () => !processExists(pid),
    `${description} (${pid})`,
    { timeoutMs },
  );
}

function killProcessGroup(pid, signal) {
  if (!pid) return;
  try {
    process.kill(-pid, signal);
  } catch (error) {
    if (error?.code !== "ESRCH") throw error;
  }
}

test("parseArguments rejects duplicate and unsafe profile/snapshot options", () => {
  const snapshot = join(tmpdir(), "easy-complete", ".specs-inputs.test", "specs-ir");

  assert.deepEqual(
    parseArguments(["--snapshot", snapshot, "--profile", "dist"]),
    { profile: "dist", snapshot: resolve(snapshot) },
  );
  assert.throws(
    () =>
      parseArguments([
        "--profile",
        "dist",
        "--snapshot",
        snapshot,
        "--profile",
        "dev",
      ]),
    /duplicate option --profile/,
  );
  assert.throws(
    () =>
      parseArguments([
        "--profile",
        "dist",
        "--snapshot",
        snapshot,
        "--snapshot",
        join(tmpdir(), "other-specs-ir"),
      ]),
    /duplicate option --snapshot/,
  );
  assert.throws(
    () => parseArguments(["--profile", "dist/release", "--snapshot", snapshot]),
    /cargo profile contains unsupported characters/,
  );
  assert.throws(
    () => parseArguments(["--profile", "dist", "--snapshot", "build/specs-ir"]),
    /snapshot must be an absolute path without NUL/,
  );
  assert.throws(
    () =>
      parseArguments([
        "--profile",
        "dist",
        "--snapshot",
        `${snapshot}\0unsafe`,
      ]),
    /snapshot must be an absolute path without NUL/,
  );
  assert.deepEqual(
    parseArguments(["--verify-snapshot", snapshot]),
    { verifySnapshot: resolve(snapshot) },
  );
  assert.throws(
    () => parseArguments(["--verify-snapshot", snapshot, "extra"]),
    /usage: build-spec-inputs.mjs --verify-snapshot/,
  );
});

function identity(info) {
  return { dev: String(info.dev), ino: String(info.ino) };
}

test("removeOwnedDirectory never removes a replaced directory", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "easy-complete-build-inputs-cleanup-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const removable = join(root, "removable");
  await mkdir(removable);
  await writeFile(join(removable, "owned"), "owned\n");
  await removeOwnedDirectory(removable, identity(await lstat(removable)));
  await assert.rejects(lstat(removable), { code: "ENOENT" });

  const owned = join(root, "owned");
  const foreign = join(root, "foreign");
  await mkdir(owned);
  await mkdir(foreign);
  await writeFile(join(foreign, "do-not-delete"), "foreign\n");
  const ownedIdentity = identity(await lstat(owned));

  await rm(owned, { recursive: true });
  await rename(foreign, owned);
  await assert.rejects(
    removeOwnedDirectory(owned, ownedIdentity),
    /refusing to remove replaced build input/,
  );
  assert.equal(await readFile(join(owned, "do-not-delete"), "utf8"), "foreign\n");
});

test("snapshot manifest pins directory identities, binary digests, and IR pair digest", async (t) => {
  await mkdir(join(repoDir, "build"), { recursive: true });
  const workRoot = await mkdtemp(
    join(repoDir, "build", ".specs-inputs.snapshot-test-"),
  );
  t.after(() => rm(workRoot, { recursive: true, force: true }));
  const source = join(workRoot, "source");
  const snapshot = join(workRoot, "specs-ir");
  const binaries = join(workRoot, "bin");
  await mkdir(source);
  await mkdir(snapshot);
  await mkdir(binaries);
  await writeFile(join(source, "source.js"), "export default {};\n");
  await writeFile(join(snapshot, "index.json"), "{}\n");
  const marker = await createPairMarker({ sourceRoot: source, irRoot: snapshot });
  await writePairMarker(snapshot, marker);
  for (const name of ["fastab", "ftab", "fastabterm", "fig_input_method"]) {
    await writeFile(join(binaries, name), `${name}\n`);
    await chmod(join(binaries, name), 0o755);
  }
  const buildRoot = join(repoDir, "build");
  const manifest = await writeSnapshotManifest({
    snapshot,
    binaries,
    buildRootIdentity: identity(await lstat(buildRoot)),
    workRootIdentity: identity(await lstat(workRoot)),
    snapshotIdentity: identity(await lstat(snapshot)),
    binariesIdentity: identity(await lstat(binaries)),
    pairSha256: marker.pairSha256,
  });
  assert.equal(manifest.snapshot.pairSha256, marker.pairSha256);
  await verifySnapshotForAssembly(snapshot);

  await writeFile(join(binaries, "ftab"), "tampered\n");
  await chmod(join(binaries, "ftab"), 0o755);
  await assert.rejects(
    verifySnapshotForAssembly(snapshot),
    /binary snapshot ftab changed before assembly/,
  );
  await writeFile(join(binaries, "ftab"), "ftab\n");
  await chmod(join(binaries, "ftab"), 0o755);
  await writeFile(join(snapshot, "index.json"), "{\"tampered\":true}\n");
  await assert.rejects(
    verifySnapshotForAssembly(snapshot),
    /IR tree does not match|IR snapshot pair digest changed/,
  );
});

test("copyTreeContents fills a reserved directory without nesting or overwrite", async (t) => {
  const root = await mkdtemp(
    join(tmpdir(), "easy-complete-build-inputs-copy-"),
  );
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "source");
  const destination = join(root, "destination");
  await mkdir(join(source, "nested", "empty"), { recursive: true });
  await mkdir(destination);
  await writeFile(join(source, "index.json"), "source-index\n");
  await writeFile(join(source, "nested", "hook.js"), "source-hook\n");

  await copyTreeContents(source, destination);
  assert.equal(await readFile(join(destination, "index.json"), "utf8"), "source-index\n");
  assert.equal(
    await readFile(join(destination, "nested", "hook.js"), "utf8"),
    "source-hook\n",
  );
  assert.deepEqual(await readdir(join(destination, "nested", "empty")), []);
  assert.equal((await readdir(destination)).includes("source"), false);

  await assert.rejects(copyTreeContents(source, destination), {
    code: "ERR_FS_CP_EEXIST",
  });
  assert.equal(await readFile(join(destination, "index.json"), "utf8"), "source-index\n");
});

test(
  "runManagedChild forwards SIGTERM to the process group and waits for its grandchild",
  { skip: process.platform === "win32", timeout: 15_000 },
  async (t) => {
    const root = await mkdtemp(join(tmpdir(), "easy-complete-build-inputs-lifecycle-"));
    const statePath = join(root, "state.log");
    let runner;
    let managedChildPid;
    let grandchildPid;
    let runnerClosed;
    let runnerError;
    const outputLines = [];

    const grandchildSource = `
      import { appendFile } from "node:fs/promises";
      const statePath = ${JSON.stringify(statePath)};
      let terminating = false;
      await appendFile(statePath, "ready:" + process.pid + "\\n");
      process.stdout.write("GRANDCHILD_READY:" + process.pid + "\\n");
      process.on("SIGTERM", () => {
        if (terminating) return;
        terminating = true;
        setTimeout(() => {
          appendFile(statePath, "terminated:" + process.pid + "\\n")
            .then(() => {
              process.stdout.write(
                "GRANDCHILD_TERMINATED:" + process.pid + "\\n",
                () => process.exit(0),
              );
            })
            .catch(() => process.exit(1));
        }, 500);
      });
      setInterval(() => {}, 1_000);
    `;
    const managedChildSource = `
      import { spawn } from "node:child_process";
      const grandchild = spawn(
        process.execPath,
        ["--input-type=module", "-e", ${JSON.stringify(grandchildSource)}],
        { stdio: ["ignore", "inherit", "inherit"] },
      );
      let stopping = false;
      process.stdout.write("MANAGED_CHILD_READY:" + process.pid + "\\n");
      process.on("SIGTERM", () => {
        stopping = true;
      });
      grandchild.once("error", (error) => {
        process.stderr.write(String(error));
        process.exitCode = 1;
      });
      grandchild.once("close", (code) => {
        if (stopping) {
          process.stdout.write("CHILD_OBSERVED_GRANDCHILD_EXIT\\n");
        }
        process.exitCode = code === 0 ? 0 : 1;
      });
    `;
    const runnerSource = `
      import { runManagedChild } from ${JSON.stringify(helperUrl)};
      try {
        await runManagedChild(
          process.execPath,
          ["--input-type=module", "-e", ${JSON.stringify(managedChildSource)}],
          { onStdoutLine: (line) => process.stdout.write(line + "\\n") },
        );
        process.stdout.write("RUNNER_UNEXPECTED_SUCCESS\\n");
        process.exitCode = 2;
      } catch (error) {
        process.stdout.write("RUNNER_DONE:" + error.message + "\\n");
      }
    `;

    runner = spawn(
      process.execPath,
      ["--input-type=module", "-e", runnerSource],
      { cwd: root, stdio: ["ignore", "pipe", "pipe"] },
    );
    const output = createInterface({
      input: runner.stdout,
      crlfDelay: Infinity,
    });
    output.on("line", (line) => {
      outputLines.push(line);
      const match = /^(?:MANAGED_CHILD_READY|GRANDCHILD_READY):([0-9]+)$/.exec(line);
      if (match) {
        if (line.startsWith("MANAGED_CHILD_READY:")) managedChildPid = Number(match[1]);
        else grandchildPid = Number(match[1]);
      }
    });
    runner.stderr.resume();
    runner.once("error", (error) => {
      runnerError = error;
    });
    runner.once("close", (code, signal) => {
      runnerClosed = { code, signal };
      output.close();
    });

    t.after(async () => {
      output.close();
      if (runner.exitCode === null && runner.signalCode === null) {
        runner.kill("SIGKILL");
      }
      if (managedChildPid) killProcessGroup(managedChildPid, "SIGKILL");
      if (grandchildPid && processExists(grandchildPid)) {
        process.kill(grandchildPid, "SIGKILL");
      }
      if (runner.pid) {
        await waitForProcessExit(runner.pid, "runner cleanup", 2_000).catch(() => {});
      }
      if (grandchildPid) {
        await waitForProcessExit(grandchildPid, "grandchild cleanup", 2_000).catch(() => {});
      }
      await rm(root, { recursive: true, force: true });
    });

    await waitFor(() => grandchildPid, "grandchild to start");
    assert.equal(runnerError, undefined);
    assert.equal(runner.kill("SIGTERM"), true);
    await delay(100);
    assert.equal(runnerClosed, undefined, "runner exited before grandchild shutdown completed");
    await waitFor(() => runnerClosed, "runner to finish after group shutdown");
    assert.equal(runnerClosed.code, 0);
    assert.equal(runnerClosed.signal, null);
    const terminatedIndex = outputLines.findIndex((line) =>
      line.startsWith("GRANDCHILD_TERMINATED:"),
    );
    const doneIndex = outputLines.findIndex((line) => line.startsWith("RUNNER_DONE:"));
    assert.notEqual(terminatedIndex, -1, outputLines.join("\\n"));
    assert.notEqual(doneIndex, -1, outputLines.join("\\n"));
    assert.ok(
      terminatedIndex < doneIndex,
      "runner settled before the grandchild reported termination",
    );
    assert.equal(
      await readFile(statePath, "utf8"),
      `ready:${grandchildPid}\nterminated:${grandchildPid}\n`,
    );
    await waitForProcessExit(grandchildPid, "grandchild to terminate");
  },
);
