#!/usr/bin/env node
/** Atomically publish one fully assembled macOS application bundle. */
import { spawn } from "node:child_process";
import { lstat, realpath } from "node:fs/promises";
import {
  basename,
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { acquirePairLock } from "./spec-pair.mjs";

const repoDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const buildRoot = join(repoDir, "build");
const expectedFinal = join(buildRoot, "Fastab.app");
const publishLockPath = join(buildRoot, ".app-publish.lock");

function parseArguments(argv) {
  const values = new Map();
  const allowed = new Set(["--staging", "--final", "--swap-helper"]);
  for (let index = 0; index < argv.length; index += 2) {
    const name = argv[index];
    const value = argv[index + 1];
    if (!allowed.has(name) || value === undefined) {
      throw new Error(
        "usage: publish-app-bundle.mjs --staging <app> --final <app> --swap-helper <binary>",
      );
    }
    if (values.has(name)) throw new Error(`duplicate option ${name}`);
    if (!isAbsolute(value) || value.includes("\0")) {
      throw new Error(`${name} must be an absolute path without NUL`);
    }
    values.set(name, resolve(value));
  }
  if (values.size !== allowed.size) {
    throw new Error(
      "usage: publish-app-bundle.mjs --staging <app> --final <app> --swap-helper <binary>",
    );
  }
  return {
    staging: values.get("--staging"),
    final: values.get("--final"),
    swapHelper: values.get("--swap-helper"),
  };
}

function isWithin(root, target) {
  const path = relative(root, target);
  return path !== "" && !isAbsolute(path) && path !== ".." && !path.startsWith(`..${sep}`);
}

function identity(info) {
  return { dev: String(info.dev), ino: String(info.ino) };
}

function sameIdentity(left, right) {
  return left.dev === right.dev && left.ino === right.ino;
}

async function directoryIdentity(path, label, { optional = false } = {}) {
  try {
    const info = await lstat(path);
    if (info.isSymbolicLink() || !info.isDirectory()) {
      throw new Error(`${label} must be a regular directory`);
    }
    return identity(info);
  } catch (error) {
    if (optional && error?.code === "ENOENT") return null;
    throw error;
  }
}

async function validatePaths(
  { staging, final, swapHelper },
  { publicationRoot = buildRoot, expectedFinalPath = expectedFinal } = {},
) {
  const canonicalBuildRoot = await realpath(publicationRoot);
  if (canonicalBuildRoot !== resolve(publicationRoot)) {
    throw new Error("build directory must not be a symbolic link");
  }
  const workRoot = dirname(staging);
  const canonicalWorkRoot = await realpath(workRoot);
  if (
    canonicalWorkRoot !== resolve(workRoot) ||
    !isWithin(canonicalBuildRoot, canonicalWorkRoot) ||
    !basename(canonicalWorkRoot).startsWith(".specs-inputs.") ||
    basename(staging) !== "Fastab.app" ||
    final !== resolve(expectedFinalPath) ||
    dirname(swapHelper) !== canonicalWorkRoot
  ) {
    throw new Error("app publication paths are outside the owned build directory");
  }
  const helperInfo = await lstat(swapHelper);
  if (
    helperInfo.isSymbolicLink() ||
    !helperInfo.isFile() ||
    (helperInfo.mode & 0o111) === 0
  ) {
    throw new Error("atomic swap helper must be an executable regular file");
  }
  return {
    stagingIdentity: await directoryIdentity(staging, "staging app"),
    finalIdentity: await directoryIdentity(final, "final app", {
      optional: true,
    }),
  };
}

async function runRenameHelper(swapHelper, operation, source, destination) {
  await new Promise((resolveRun, rejectRun) => {
    const child = spawn(swapHelper, [operation, source, destination], {
      cwd: repoDir,
      shell: false,
      stdio: "inherit",
    });
    child.once("error", rejectRun);
    child.once("close", (code, signal) => {
      if (code === 0) resolveRun();
      else {
        rejectRun(
          new Error(
            `atomic app ${operation} failed (${signal ? `signal ${signal}` : `exit ${code}`})`,
          ),
        );
      }
    });
  });
}

async function publish(
  options,
  {
    publicationRoot = buildRoot,
    expectedFinalPath = expectedFinal,
    lockPath = publishLockPath,
  } = {},
) {
  const lock = await acquirePairLock(lockPath);
  try {
    const before = await validatePaths(options, {
      publicationRoot,
      expectedFinalPath,
    });
    if (before.finalIdentity) {
      await runRenameHelper(
        options.swapHelper,
        "swap",
        options.staging,
        options.final,
      );
      let published;
      let displaced;
      try {
        [published, displaced] = await Promise.all([
          directoryIdentity(options.final, "published app"),
          directoryIdentity(options.staging, "displaced app"),
        ]);
      } catch (inspectionError) {
        try {
          // A non-cooperating process may replace/remove either pathname
          // after the atomic exchange. Attempt the inverse exchange even when
          // the current entries can no longer be classified as directories.
          await runRenameHelper(
            options.swapHelper,
            "swap",
            options.staging,
            options.final,
          );
          const [restoredStaging, restoredFinal] = await Promise.all([
            directoryIdentity(options.staging, "restored staging app"),
            directoryIdentity(options.final, "restored final app"),
          ]);
          if (
            !sameIdentity(restoredStaging, before.stagingIdentity) ||
            !sameIdentity(restoredFinal, before.finalIdentity)
          ) {
            throw new Error("unclassifiable app swap could not restore identities");
          }
        } catch (rollbackError) {
          throw new AggregateError(
            [inspectionError, rollbackError],
            "atomic app swap could not be inspected or restored safely",
          );
        }
        throw inspectionError;
      }
      if (
        !sameIdentity(published, before.stagingIdentity) ||
        !sameIdentity(displaced, before.finalIdentity)
      ) {
        const mismatch = new Error(
          "atomic app swap did not exchange the expected directories",
        );
        try {
          // The exchange itself is atomic, but an uncooperative process can
          // replace either pathname between validation and the syscall. Put
          // the two observed directories back before reporting the mismatch;
          // build-app.sh preserves the work root on every publication error.
          await runRenameHelper(
            options.swapHelper,
            "swap",
            options.staging,
            options.final,
          );
          const [restoredStaging, restoredFinal] = await Promise.all([
            directoryIdentity(options.staging, "restored staging app"),
            directoryIdentity(options.final, "restored final app"),
          ]);
          if (
            !sameIdentity(restoredStaging, published) ||
            !sameIdentity(restoredFinal, displaced)
          ) {
            throw new Error("atomic app swap rollback changed directory identity");
          }
        } catch (rollbackError) {
          throw new AggregateError(
            [mismatch, rollbackError],
            "atomic app swap mismatched and could not be restored safely",
          );
        }
        throw mismatch;
      }
    } else {
      // RENAME_EXCL is the no-clobber counterpart to RENAME_SWAP. A plain
      // rename would remove an empty directory that raced into the final path.
      await runRenameHelper(
        options.swapHelper,
        "exclusive",
        options.staging,
        options.final,
      );
      let published;
      try {
        published = await directoryIdentity(options.final, "published app");
      } catch (inspectionError) {
        try {
          await runRenameHelper(
            options.swapHelper,
            "exclusive",
            options.final,
            options.staging,
          );
          const [restoredStaging, restoredFinal] = await Promise.all([
            directoryIdentity(options.staging, "restored staging app"),
            directoryIdentity(options.final, "restored final app", {
              optional: true,
            }),
          ]);
          if (
            !sameIdentity(restoredStaging, before.stagingIdentity) ||
            restoredFinal !== null
          ) {
            throw new Error(
              "unclassifiable exclusive publication could not restore identities",
            );
          }
        } catch (rollbackError) {
          throw new AggregateError(
            [inspectionError, rollbackError],
            "exclusive app publication could not be inspected or restored safely",
          );
        }
        throw inspectionError;
      }
      if (!sameIdentity(published, before.stagingIdentity)) {
        const mismatch = new Error("app publication changed directory identity");
        try {
          await runRenameHelper(
            options.swapHelper,
            "exclusive",
            options.final,
            options.staging,
          );
          const [restored, restoredFinal] = await Promise.all([
            directoryIdentity(options.staging, "restored staging app"),
            directoryIdentity(options.final, "restored final app", {
              optional: true,
            }),
          ]);
          if (!sameIdentity(restored, published) || restoredFinal !== null) {
            throw new Error(
              "exclusive app publication rollback changed identity or left a final app",
            );
          }
        } catch (rollbackError) {
          throw new AggregateError(
            [mismatch, rollbackError],
            "exclusive app publication mismatched and could not be restored safely",
          );
        }
        throw mismatch;
      }
    }
  } finally {
    await lock.release();
  }
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  await validatePaths(options);
  await publish(options);
}

const isMain =
  process.argv[1] &&
  pathToFileURL(resolve(process.argv[1])).href === import.meta.url;

if (isMain) await main();

export { parseArguments, publish };
