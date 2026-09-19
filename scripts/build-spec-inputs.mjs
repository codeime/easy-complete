#!/usr/bin/env node
/**
 * Build the native binaries while the bundled source/IR generation is locked.
 *
 * `ec_gpui` embeds icons from bundle/specs with `include_bytes!`, while the
 * application ships bundle/specs-ir as a lazy runtime resource.  Holding the
 * publication lock across rustc and the IR snapshot prevents a concurrent
 * spec sync from making those two inputs come from different generations.
 * This helper is build tooling only and is never copied into the application.
 */
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import {
  constants,
  cp,
  lstat,
  mkdir,
  open,
  readdir,
  realpath,
  rename,
  rm,
} from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath, pathToFileURL } from "node:url";

import { auditSpecsHooks } from "./audit-spec-hooks.mjs";
import {
  PAIR_LOCK_NAME,
  verifyPair,
  withPairLock,
} from "./spec-pair.mjs";

const repoDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const sourceRoot = join(repoDir, "bundle", "specs");
const irRoot = join(repoDir, "bundle", "specs-ir");
const lockPath = join(repoDir, "bundle", PAIR_LOCK_NAME);
const buildRoot = join(repoDir, "build");
const FORCE_KILL_AFTER_MS = 5_000;
const SNAPSHOT_MANIFEST_NAME = ".build-input-snapshot.json";
const SNAPSHOT_MANIFEST_FORMAT = 1;
const SNAPSHOT_MANIFEST_KIND = "easy-complete-build-input-snapshot";
const EXPECTED_BINARIES = new Map([
  ["fastab", join(repoDir, "crates", "fig_desktop", "Cargo.toml")],
  ["ftab", join(repoDir, "crates", "ec_cli", "Cargo.toml")],
  ["fastabterm", join(repoDir, "crates", "figterm", "Cargo.toml")],
  [
    "fig_input_method",
    join(repoDir, "crates", "fig_input_method", "Cargo.toml"),
  ],
]);

export function parseArguments(argv) {
  if (argv[0] === "--verify-snapshot") {
    if (argv.length !== 2) {
      throw new Error("usage: build-spec-inputs.mjs --verify-snapshot <snapshot-path>");
    }
    const snapshot = argv[1];
    if (!isAbsolute(snapshot) || snapshot.includes("\0")) {
      throw new Error("snapshot must be an absolute path without NUL");
    }
    return { verifySnapshot: resolve(snapshot) };
  }
  const values = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const name = argv[index];
    const value = argv[index + 1];
    if (
      !name ||
      value === undefined ||
      !["--profile", "--snapshot"].includes(name)
    ) {
      throw new Error(
        "usage: build-spec-inputs.mjs --profile <cargo-profile> --snapshot <empty-path>",
      );
    }
    if (values.has(name)) throw new Error(`duplicate option ${name}`);
    values.set(name, value);
  }
  if (values.size !== 2) {
    throw new Error(
      "usage: build-spec-inputs.mjs --profile <cargo-profile> --snapshot <empty-path>",
    );
  }
  const profile = values.get("--profile");
  const snapshot = values.get("--snapshot");
  if (!/^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/.test(profile)) {
    throw new Error("cargo profile contains unsupported characters");
  }
  if (!isAbsolute(snapshot) || snapshot.includes("\0")) {
    throw new Error("snapshot must be an absolute path without NUL");
  }
  return { profile, snapshot: resolve(snapshot) };
}

function isWithin(root, target) {
  const path = relative(root, target);
  return path !== "" && !isAbsolute(path) && path !== ".." && !path.startsWith(`..${sep}`);
}

async function validateSnapshotParent(snapshot) {
  await mkdir(buildRoot, { recursive: true });
  const canonicalBuildRoot = await realpath(buildRoot);
  if (canonicalBuildRoot !== resolve(buildRoot)) {
    throw new Error("build directory must not be a symbolic link");
  }
  const parent = dirname(snapshot);
  const canonicalParent = await realpath(parent);
  if (
    canonicalParent !== resolve(parent) ||
    !isWithin(canonicalBuildRoot, canonicalParent) ||
    !basename(canonicalParent).startsWith(".specs-inputs.") ||
    basename(snapshot) !== "specs-ir"
  ) {
    throw new Error("snapshot must be specs-ir inside a build/.specs-inputs.* directory");
  }
  return parent;
}

async function validateSnapshotDestination(snapshot) {
  await validateSnapshotParent(snapshot);
  try {
    await lstat(snapshot);
  } catch (error) {
    if (error?.code === "ENOENT") return;
    throw error;
  }
  throw new Error("snapshot destination already exists");
}

function directoryIdentity(info) {
  return { dev: String(info.dev), ino: String(info.ino) };
}

function sameIdentity(left, right) {
  return left.dev === right.dev && left.ino === right.ino;
}

async function reserveDirectory(path) {
  await mkdir(path, { mode: 0o700 });
  const info = await lstat(path);
  if (info.isSymbolicLink() || !info.isDirectory()) {
    throw new Error(`reserved build input is not a directory: ${path}`);
  }
  return directoryIdentity(info);
}

export async function removeOwnedDirectory(path, identity) {
  if (!identity) return;
  let info;
  try {
    info = await lstat(path);
  } catch (error) {
    if (error?.code === "ENOENT") return;
    throw error;
  }
  if (
    info.isSymbolicLink() ||
    !info.isDirectory() ||
    !sameIdentity(directoryIdentity(info), identity)
  ) {
    throw new Error(`refusing to remove replaced build input: ${path}`);
  }
  const quarantine = join(
    dirname(path),
    `.${basename(path)}.cleanup-${process.pid}-${randomUUID()}`,
  );
  try {
    await rename(path, quarantine);
  } catch (error) {
    if (error?.code === "ENOENT") return;
    throw error;
  }

  let isolated;
  try {
    isolated = await lstat(quarantine);
  } catch (error) {
    throw new Error(`could not inspect isolated build input: ${quarantine}`, {
      cause: error,
    });
  }
  if (
    isolated.isSymbolicLink() ||
    !isolated.isDirectory() ||
    !sameIdentity(directoryIdentity(isolated), identity)
  ) {
    // The pathname was replaced between the first lstat and the rename. Do
    // not recursively remove an unknown tree. Restore it only when the
    // original pathname is still vacant; otherwise leave both entries for
    // the caller to preserve and inspect.
    try {
      await lstat(path);
    } catch (error) {
      if (error?.code === "ENOENT") {
        await rename(quarantine, path).catch(() => {});
      }
    }
    throw new Error(`refusing to remove replaced build input: ${path}`);
  }
  await rm(quarantine, { recursive: true, force: false });
}

async function assertOwnedDirectory(path, identity, label) {
  const info = await lstat(path);
  if (
    info.isSymbolicLink() ||
    !info.isDirectory() ||
    !sameIdentity(directoryIdentity(info), identity)
  ) {
    throw new Error(`${label} directory identity changed: ${path}`);
  }
}

function signalProcessGroup(child, signal) {
  if (!child.pid) return;
  try {
    if (process.platform === "win32") child.kill(signal);
    else process.kill(-child.pid, signal);
  } catch (error) {
    if (error?.code !== "ESRCH") throw error;
  }
}

function processGroupExists(child) {
  if (!child.pid || process.platform === "win32") return false;
  try {
    process.kill(-child.pid, 0);
    return true;
  } catch (error) {
    if (error?.code === "ESRCH") return false;
    if (error?.code === "EPERM") return true;
    throw error;
  }
}

async function waitForProcessGroupExit(child) {
  if (!processGroupExists(child)) return;
  const deadline = Date.now() + FORCE_KILL_AFTER_MS;
  while (Date.now() < deadline) {
    await new Promise((resolveWait) => setTimeout(resolveWait, 25));
    if (!processGroupExists(child)) return;
  }
  signalProcessGroup(child, "SIGKILL");
  const killDeadline = Date.now() + 1_000;
  while (Date.now() < killDeadline) {
    await new Promise((resolveWait) => setTimeout(resolveWait, 25));
    if (!processGroupExists(child)) return;
  }
  // SIGKILL has been delivered to every member of the group. A remaining
  // entry can only be a zombie waiting for its new parent to reap it, or a
  // task with SIGKILL pending while it leaves an uninterruptible kernel wait;
  // neither can resume userspace work against the spec tree.
  process.stderr.write(
    `warning: managed process group ${child.pid} remains visible after SIGKILL; continuing after userspace execution was terminated\n`,
  );
}

/**
 * Run one child in its own process group. SIGINT/SIGTERM delivered only to
 * this Node process are forwarded to the whole group, and the promise does
 * not settle until every direct child handle has closed. That ordering keeps
 * the pair lock alive until Cargo/rustc have actually stopped.
 */
export async function runManagedChild(
  command,
  args,
  { cwd = repoDir, env = process.env, onStdoutLine = () => {} } = {},
) {
  const child = spawn(command, args, {
    cwd,
    env,
    detached: process.platform !== "win32",
    shell: false,
    stdio: ["ignore", "pipe", "inherit"],
  });
  let spawnError = null;
  let outputError = null;
  let stopError = null;
  let interruptedBy = null;
  let forceKillTimer = null;

  const scheduleForceKill = () => {
    if (forceKillTimer) return;
    forceKillTimer = setTimeout(() => {
      try {
        signalProcessGroup(child, "SIGKILL");
      } catch {
        // The close/error path below reports the original failure.
      }
    }, FORCE_KILL_AFTER_MS);
  };
  const requestStop = (signal, { interrupted = false } = {}) => {
    try {
      if (interrupted) {
        if (interruptedBy) {
          signalProcessGroup(child, "SIGKILL");
          return;
        }
        interruptedBy = signal;
      }
      signalProcessGroup(child, signal);
      scheduleForceKill();
    } catch (error) {
      stopError ??= error;
    }
  };
  const onSigint = () => requestStop("SIGINT", { interrupted: true });
  const onSigterm = () => requestStop("SIGTERM", { interrupted: true });
  process.on("SIGINT", onSigint);
  process.on("SIGTERM", onSigterm);

  child.once("error", (error) => {
    spawnError = error;
  });
  const closed = new Promise((resolveClose) => {
    child.once("close", (code, signal) => resolveClose({ code, signal }));
  });
  const output = (async () => {
    const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
    try {
      for await (const line of lines) await onStdoutLine(line);
    } catch (error) {
      outputError = error;
      requestStop("SIGTERM");
    } finally {
      lines.close();
    }
  })();

  try {
    const status = await closed;
    await output;
    await waitForProcessGroupExit(child);
    if (spawnError) throw spawnError;
    if (outputError) throw outputError;
    if (stopError) throw stopError;
    if (interruptedBy) {
      throw new Error(`managed child interrupted by ${interruptedBy}`);
    }
    if (status.code !== 0) {
      throw new Error(
        `managed child failed (${status.signal ? `signal ${status.signal}` : `exit ${status.code}`})`,
      );
    }
    return status;
  } finally {
    if (forceKillTimer) clearTimeout(forceKillTimer);
    process.off("SIGINT", onSigint);
    process.off("SIGTERM", onSigterm);
  }
}

function collectCargoMessage(state, line) {
  if (!line) return;
  let message;
  try {
    message = JSON.parse(line);
  } catch (error) {
    throw new Error(`cargo emitted a non-JSON stdout line: ${line}`, {
      cause: error,
    });
  }
  if (message.reason === "compiler-message") {
    if (message.message?.rendered) process.stderr.write(message.message.rendered);
    return;
  }
  if (message.reason === "build-finished") {
    state.finished = true;
    state.success = message.success === true;
    return;
  }
  if (
    message.reason !== "compiler-artifact" ||
    !Array.isArray(message.target?.kind) ||
    !message.target.kind.includes("bin") ||
    !EXPECTED_BINARIES.has(message.target.name)
  ) {
    return;
  }
  const expectedManifest = EXPECTED_BINARIES.get(message.target.name);
  if (
    typeof message.manifest_path !== "string" ||
    resolve(message.manifest_path) !== resolve(expectedManifest)
  ) {
    throw new Error(
      `cargo artifact ${message.target.name} came from an unexpected manifest`,
    );
  }
  if (typeof message.executable !== "string" || !isAbsolute(message.executable)) {
    throw new Error(`cargo artifact ${message.target.name} has no executable path`);
  }
  const executable = resolve(message.executable);
  const previous = state.artifacts.get(message.target.name);
  if (previous && previous !== executable) {
    throw new Error(`cargo emitted conflicting paths for ${message.target.name}`);
  }
  state.artifacts.set(message.target.name, executable);
}

async function runCargo(profile) {
  const args = [
    "build",
    "--locked",
    "--profile",
    profile,
    "--message-format=json-render-diagnostics",
    "-p",
    "fig_desktop",
    "-p",
    "figterm",
    "-p",
    "ec_cli",
    "-p",
    "fig_input_method",
  ];
  const state = { artifacts: new Map(), finished: false, success: false };
  try {
    await runManagedChild("cargo", args, {
      onStdoutLine: (line) => collectCargoMessage(state, line),
    });
  } catch (error) {
    throw new Error(`cargo build failed: ${error.message}`, { cause: error });
  }
  if (!state.finished || !state.success) {
    throw new Error("cargo did not emit a successful build-finished message");
  }
  const missing = [...EXPECTED_BINARIES.keys()].filter(
    (name) => !state.artifacts.has(name),
  );
  if (missing.length) {
    throw new Error(`cargo did not emit executable artifacts: ${missing.join(", ")}`);
  }
  return state.artifacts;
}

async function digestHandle(handle, size) {
  const hash = createHash("sha256");
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  let position = 0;
  while (position < size) {
    const length = Math.min(buffer.length, size - position);
    const { bytesRead } = await handle.read(buffer, 0, length, position);
    if (bytesRead === 0) {
      throw new Error("cargo artifact became shorter while reading");
    }
    hash.update(buffer.subarray(0, bytesRead));
    position += bytesRead;
  }
  return hash.digest("hex");
}

async function captureRegularFileSnapshot(path, label) {
  const beforeOpen = await lstat(path);
  if (beforeOpen.isSymbolicLink() || !beforeOpen.isFile()) {
    throw new Error(`${label} is not a regular file: ${path}`);
  }
  const handle = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const opened = await handle.stat();
    if (
      !opened.isFile() ||
      opened.dev !== beforeOpen.dev ||
      opened.ino !== beforeOpen.ino ||
      !Number.isSafeInteger(opened.size) ||
      opened.size < 0
    ) {
      throw new Error(`${label} changed identity while opening: ${path}`);
    }
    const sha256 = await digestHandle(handle, opened.size);
    const after = await handle.stat();
    if (
      after.dev !== opened.dev ||
      after.ino !== opened.ino ||
      after.size !== opened.size ||
      (after.mode & 0o777) !== (opened.mode & 0o777)
    ) {
      throw new Error(`${label} changed while being read: ${path}`);
    }
    return {
      identity: directoryIdentity(opened),
      mode: opened.mode & 0o777,
      size: opened.size,
      sha256,
    };
  } finally {
    await handle.close().catch(() => {});
  }
}

function isSnapshotIdentity(value) {
  return (
    value &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    typeof value.dev === "string" &&
    typeof value.ino === "string"
  );
}

function isSnapshotDigest(value) {
  return typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
}

function assertSnapshotFileRecord(value, label) {
  if (
    !value ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    !isSnapshotIdentity(value.identity) ||
    !Number.isSafeInteger(value.mode) ||
    value.mode < 0 ||
    value.mode > 0o777 ||
    !Number.isSafeInteger(value.size) ||
    value.size < 0 ||
    !isSnapshotDigest(value.sha256)
  ) {
    throw new Error(`invalid binary snapshot record: ${label}`);
  }
}

function sameFileSnapshot(left, right) {
  return (
    sameIdentity(left.identity, right.identity) &&
    left.mode === right.mode &&
    left.size === right.size &&
    left.sha256 === right.sha256
  );
}

async function readStableJsonFile(path, label) {
  const beforeOpen = await lstat(path);
  if (beforeOpen.isSymbolicLink() || !beforeOpen.isFile()) {
    throw new Error(`${label} is not a regular file: ${path}`);
  }
  const handle = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const opened = await handle.stat();
    if (
      !opened.isFile() ||
      opened.dev !== beforeOpen.dev ||
      opened.ino !== beforeOpen.ino
    ) {
      throw new Error(`${label} changed identity while opening: ${path}`);
    }
    const text = await handle.readFile("utf8");
    const after = await handle.stat();
    if (
      after.dev !== opened.dev ||
      after.ino !== opened.ino ||
      after.size !== opened.size
    ) {
      throw new Error(`${label} changed while being read: ${path}`);
    }
    try {
      return JSON.parse(text);
    } catch (error) {
      throw new Error(`invalid ${label}: ${path}`, { cause: error });
    }
  } finally {
    await handle.close().catch(() => {});
  }
}

async function snapshotManifestData({
  snapshot,
  binaries,
  buildRootIdentity,
  workRootIdentity,
  snapshotIdentity,
  binariesIdentity,
  pairSha256,
}) {
  await assertOwnedDirectory(buildRoot, buildRootIdentity, "build root");
  await assertOwnedDirectory(dirname(snapshot), workRootIdentity, "build work root");
  await assertOwnedDirectory(snapshot, snapshotIdentity, "IR snapshot");
  await assertOwnedDirectory(binaries, binariesIdentity, "binary snapshot");
  if (!isSnapshotDigest(pairSha256)) {
    throw new Error("invalid pair digest for build input snapshot");
  }

  const files = {};
  for (const name of EXPECTED_BINARIES.keys()) {
    await assertOwnedDirectory(binaries, binariesIdentity, "binary snapshot");
    files[name] = await captureRegularFileSnapshot(
      join(binaries, name),
      `binary snapshot ${name}`,
    );
  }
  await assertOwnedDirectory(binaries, binariesIdentity, "binary snapshot");
  await assertOwnedDirectory(snapshot, snapshotIdentity, "IR snapshot");
  return {
    format: SNAPSHOT_MANIFEST_FORMAT,
    kind: SNAPSHOT_MANIFEST_KIND,
    buildRoot: {
      path: resolve(buildRoot),
      identity: buildRootIdentity,
    },
    workRoot: {
      path: resolve(dirname(snapshot)),
      identity: workRootIdentity,
    },
    snapshot: {
      path: resolve(snapshot),
      identity: snapshotIdentity,
      pairSha256,
    },
    binaries: {
      path: resolve(binaries),
      identity: binariesIdentity,
      files,
    },
  };
}

export async function writeSnapshotManifest(options) {
  const manifestPath = join(dirname(options.snapshot), SNAPSHOT_MANIFEST_NAME);
  const manifest = await snapshotManifestData(options);
  const handle = await open(
    manifestPath,
    constants.O_WRONLY |
      constants.O_CREAT |
      constants.O_EXCL |
      constants.O_NOFOLLOW,
    0o600,
  );
  try {
    await handle.writeFile(`${JSON.stringify(manifest)}\n`, "utf8");
    await handle.sync();
  } finally {
    await handle.close().catch(() => {});
  }
  return manifest;
}

function validateSnapshotManifest(manifest, snapshot, binaries) {
  if (
    !manifest ||
    typeof manifest !== "object" ||
    Array.isArray(manifest) ||
    manifest.format !== SNAPSHOT_MANIFEST_FORMAT ||
    manifest.kind !== SNAPSHOT_MANIFEST_KIND
  ) {
    throw new Error("invalid build input snapshot manifest format");
  }
  for (const [name, value] of [
    ["buildRoot", manifest.buildRoot],
    ["workRoot", manifest.workRoot],
    ["snapshot", manifest.snapshot],
    ["binaries", manifest.binaries],
  ]) {
    if (
      !value ||
      typeof value !== "object" ||
      Array.isArray(value) ||
      typeof value.path !== "string" ||
      !isAbsolute(value.path) ||
      !isSnapshotIdentity(value.identity)
    ) {
      throw new Error(`invalid build input snapshot manifest ${name}`);
    }
  }
  if (
    manifest.buildRoot.path !== resolve(buildRoot) ||
    manifest.workRoot.path !== resolve(dirname(snapshot)) ||
    manifest.snapshot.path !== resolve(snapshot) ||
    manifest.binaries.path !== resolve(binaries) ||
    !isSnapshotIdentity(manifest.snapshot.identity) ||
    !isSnapshotDigest(manifest.snapshot.pairSha256) ||
    !isSnapshotIdentity(manifest.binaries.identity) ||
    !manifest.binaries.files ||
    typeof manifest.binaries.files !== "object" ||
    Array.isArray(manifest.binaries.files)
  ) {
    throw new Error("build input snapshot manifest paths or fields do not match");
  }
  const names = Object.keys(manifest.binaries.files);
  const expectedNames = [...EXPECTED_BINARIES.keys()];
  if (
    names.length !== expectedNames.length ||
    expectedNames.some((name) => !Object.hasOwn(manifest.binaries.files, name))
  ) {
    throw new Error("build input snapshot manifest has unexpected binaries");
  }
  for (const name of expectedNames) {
    assertSnapshotFileRecord(manifest.binaries.files[name], name);
  }
}

export async function verifySnapshotForAssembly(snapshotPath) {
  const snapshot = resolve(snapshotPath);
  await validateSnapshotParent(snapshot);
  const binaries = join(dirname(snapshot), "bin");
  const manifestPath = join(dirname(snapshot), SNAPSHOT_MANIFEST_NAME);
  const manifest = await readStableJsonFile(
    manifestPath,
    "build input snapshot manifest",
  );
  validateSnapshotManifest(manifest, snapshot, binaries);

  const currentBuildRoot = directoryIdentity(await lstat(buildRoot));
  const currentWorkRoot = directoryIdentity(await lstat(dirname(snapshot)));
  const currentSnapshot = directoryIdentity(await lstat(snapshot));
  const currentBinaries = directoryIdentity(await lstat(binaries));
  if (
    !sameIdentity(currentBuildRoot, manifest.buildRoot.identity) ||
    !sameIdentity(currentWorkRoot, manifest.workRoot.identity) ||
    !sameIdentity(currentSnapshot, manifest.snapshot.identity) ||
    !sameIdentity(currentBinaries, manifest.binaries.identity)
  ) {
    throw new Error("build input snapshot directory identity changed before assembly");
  }

  const marker = await verifyPair({ irRoot: snapshot, irOnly: true });
  if (marker.pairSha256 !== manifest.snapshot.pairSha256) {
    throw new Error("IR snapshot pair digest changed before assembly");
  }
  const entries = await readdir(binaries, { withFileTypes: true });
  const expectedNames = [...EXPECTED_BINARIES.keys()];
  if (
    entries.length !== expectedNames.length ||
    entries.some(
      (entry) =>
        !entry.isFile() ||
        entry.isSymbolicLink() ||
        !Object.hasOwn(manifest.binaries.files, entry.name),
    )
  ) {
    throw new Error("binary snapshot contains unexpected entries before assembly");
  }
  for (const name of expectedNames) {
    const current = await captureRegularFileSnapshot(
      join(binaries, name),
      `binary snapshot ${name}`,
    );
    if (!sameFileSnapshot(current, manifest.binaries.files[name])) {
      throw new Error(`binary snapshot ${name} changed before assembly`);
    }
  }
  await assertOwnedDirectory(dirname(snapshot), manifest.workRoot.identity, "build work root");
  await assertOwnedDirectory(snapshot, manifest.snapshot.identity, "IR snapshot");
  await assertOwnedDirectory(binaries, manifest.binaries.identity, "binary snapshot");
  return manifest;
}

async function copyHandle(sourceHandle, targetHandle, size) {
  const hash = createHash("sha256");
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  let position = 0;
  while (position < size) {
    const length = Math.min(buffer.length, size - position);
    const { bytesRead } = await sourceHandle.read(
      buffer,
      0,
      length,
      position,
    );
    if (bytesRead === 0) {
      throw new Error("cargo artifact became shorter while copying");
    }
    hash.update(buffer.subarray(0, bytesRead));
    let written = 0;
    while (written < bytesRead) {
      const result = await targetHandle.write(
        buffer,
        written,
        bytesRead - written,
        position + written,
      );
      if (result.bytesWritten === 0) {
        throw new Error("binary snapshot write made no progress");
      }
      written += result.bytesWritten;
    }
    position += bytesRead;
  }
  return hash.digest("hex");
}

async function copyBuiltBinaries(artifacts, destination, destinationIdentity) {
  for (const name of EXPECTED_BINARIES.keys()) {
    await assertOwnedDirectory(
      destination,
      destinationIdentity,
      "binary snapshot",
    );
    const source = artifacts.get(name);
    const target = join(destination, name);
    const beforeOpen = await lstat(source);
    if (beforeOpen.isSymbolicLink() || !beforeOpen.isFile()) {
      throw new Error(`cargo artifact is not a regular file: ${source}`);
    }
    const sourceHandle = await open(
      source,
      constants.O_RDONLY | constants.O_NOFOLLOW,
    );
    let targetHandle;
    try {
      const sourceInfo = await sourceHandle.stat();
      if (
        !sourceInfo.isFile() ||
        sourceInfo.dev !== beforeOpen.dev ||
        sourceInfo.ino !== beforeOpen.ino
      ) {
        throw new Error(`cargo artifact changed identity while opening: ${source}`);
      }
      targetHandle = await open(
        target,
        constants.O_RDWR |
          constants.O_CREAT |
          constants.O_EXCL |
          constants.O_NOFOLLOW,
        sourceInfo.mode & 0o777,
      );
      const copiedDigest = await copyHandle(
        sourceHandle,
        targetHandle,
        sourceInfo.size,
      );
      await targetHandle.chmod(sourceInfo.mode & 0o777);
      await targetHandle.sync();
      const [sourceDigestAfter, targetDigest, sourceAfter] = await Promise.all([
        digestHandle(sourceHandle, sourceInfo.size),
        digestHandle(targetHandle, sourceInfo.size),
        sourceHandle.stat(),
      ]);
      if (
        sourceAfter.dev !== sourceInfo.dev ||
        sourceAfter.ino !== sourceInfo.ino ||
        sourceAfter.size !== sourceInfo.size ||
        sourceDigestAfter !== copiedDigest ||
        targetDigest !== copiedDigest
      ) {
        throw new Error(`cargo artifact changed while copying: ${source}`);
      }
    } finally {
      await targetHandle?.close().catch(() => {});
      await sourceHandle.close().catch(() => {});
    }
    await assertOwnedDirectory(
      destination,
      destinationIdentity,
      "binary snapshot",
    );
  }
}

export async function copyTreeContents(
  source,
  destination,
  destinationIdentity = null,
) {
  const entries = await readdir(source, { withFileTypes: true });
  entries.sort((left, right) => Buffer.compare(Buffer.from(left.name), Buffer.from(right.name)));
  for (const entry of entries) {
    if (destinationIdentity) {
      await assertOwnedDirectory(
        destination,
        destinationIdentity,
        "IR snapshot",
      );
    }
    await cp(join(source, entry.name), join(destination, entry.name), {
      recursive: true,
      force: false,
      errorOnExist: true,
      preserveTimestamps: false,
    });
    if (destinationIdentity) {
      await assertOwnedDirectory(
        destination,
        destinationIdentity,
        "IR snapshot",
      );
    }
  }
}

function auditFailure(report) {
  return Object.entries(report.errors ?? {})
    .filter(([, entries]) => Array.isArray(entries) && entries.length)
    .map(([name, entries]) => `${name}=${entries.length}`)
    .join(", ");
}

async function auditPublishedPair() {
  const report = await auditSpecsHooks({
    sourceRoot,
    irRoot,
  });
  if (!report.ok) {
    throw new Error(`spec source/IR audit failed: ${auditFailure(report)}`);
  }
}

async function main() {
  const parsed = parseArguments(process.argv.slice(2));
  if (parsed.verifySnapshot) {
    await verifySnapshotForAssembly(parsed.verifySnapshot);
    return;
  }
  const { profile, snapshot } = parsed;
  const binaries = join(dirname(snapshot), "bin");
  await validateSnapshotDestination(snapshot);
  const buildRootIdentity = directoryIdentity(await lstat(buildRoot));
  const workRootIdentity = directoryIdentity(await lstat(dirname(snapshot)));
  let snapshotIdentity = null;
  let binariesIdentity = null;
  try {
    await withPairLock(
      lockPath,
      async () => {
        // Re-check and reserve both destinations while holding the pair lock.
        // A second helper that passed the earlier read-only check cannot reuse
        // or later delete directories owned by this invocation.
        await validateSnapshotDestination(snapshot);
        snapshotIdentity = await reserveDirectory(snapshot);
        binariesIdentity = await reserveDirectory(binaries);

        const before = await verifyPair({ sourceRoot, irRoot });
        await auditPublishedPair();
        const audited = await verifyPair({ sourceRoot, irRoot });
        if (audited.pairSha256 !== before.pairSha256) {
          throw new Error("spec source/IR generation changed during pre-build audit");
        }

        const artifacts = await runCargo(profile);

        const afterBuild = await verifyPair({ sourceRoot, irRoot });
        if (afterBuild.pairSha256 !== before.pairSha256) {
          throw new Error("spec source/IR generation changed during Rust compilation");
        }

        await copyBuiltBinaries(artifacts, binaries, binariesIdentity);
        await copyTreeContents(irRoot, snapshot, snapshotIdentity);
        const copied = await verifyPair({ irRoot: snapshot, irOnly: true });
        if (copied.pairSha256 !== before.pairSha256) {
          throw new Error("copied IR snapshot does not match the compiled source generation");
        }
        await writeSnapshotManifest({
          snapshot,
          binaries,
          buildRootIdentity,
          workRootIdentity,
          snapshotIdentity,
          binariesIdentity,
          pairSha256: copied.pairSha256,
        });
      },
      { verifyPublished: auditPublishedPair },
    );
  } catch (error) {
    const cleanupErrors = [];
    for (const [path, identity] of [
      [binaries, binariesIdentity],
      [snapshot, snapshotIdentity],
    ]) {
      try {
        await removeOwnedDirectory(path, identity);
      } catch (cleanupError) {
        cleanupErrors.push(cleanupError);
      }
    }
    if (cleanupErrors.length) {
      throw new AggregateError(
        [error, ...cleanupErrors],
        "build input preparation failed and owned snapshots could not be cleaned safely",
      );
    }
    throw error;
  }
}

const isMain =
  process.argv[1] &&
  pathToFileURL(resolve(process.argv[1])).href === import.meta.url;

if (isMain) await main();
