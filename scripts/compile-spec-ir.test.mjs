import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { compileSpecsIr } from "./compile-spec-ir.mjs";

test("compiler keeps static suggestion type metadata and string query terms", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-"));
  try {
    await writeFile(
      join(srcDir, "fixture.js"),
      `export default {
        name: "fixture",
        parserDirectives: {
          optionsMustPrecedeArguments: true,
          flagsArePosixNoncompliant: true,
          optionArgSeparators: ["=", ":"],
        },
        filterStrategy: "fuzzy",
        args: [{
          name: "value",
          filterStrategy: "not-a-strategy",
          suggestCurrentToken: false,
          optionsCanBreakVariadicArg: false,
          generators: { getQueryTerm: "/", template: "filepaths" },
          suggestions: [
            {
              name: "file.txt",
              type: "file",
              originalType: "folder",
              getQueryTerm: "/",
              args: [{ name: "required" }, { name: "optional", isOptional: true }],
            },
            {
              name: "function-query",
              getQueryTerm: () => "ignored",
            },
          ],
        },
        {
          name: "script value",
          filterStrategy: "prefix",
          suggestCurrentToken: true,
          generators: {
            script: ["printf", "ok\\n"],
            scriptTimeout: 7000,
          },
        }],
        options: [{
          name: ["-c", "--color"],
          description: "Choose a color",
          insertValue: "--colour",
          displayName: "Color",
          requiresSeparator: ":",
          shouldAddSpace: false,
          hidden: true,
          priority: 0,
          icon: "🟡",
          isDangerous: true,
          exclusiveOn: ["--no-color", "-C"],
          dependsOn: ["--config"],
          isRepeatable: 2,
          isPersistent: true,
          args: { name: "color" },
        }, {
          name: "--many",
          isRepeatable: true,
        }, {
          name: "--once",
          isRepeatable: false,
        }, {
          name: "--required",
          requiresSeparator: true,
          args: { name: "value" },
        }],
      };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    const ir = JSON.parse(await readFile(join(outDir, "fixture.json"), "utf8"));
    assert.equal(ir.args[0].getQueryTerm, "/");
    assert.equal(ir.args[0].suggestCurrentToken, false);
    assert.equal(ir.args[0].optionsCanBreakVariadicArg, false);
    assert.deepEqual(ir.parserDirectives, {
      optionsMustPrecedeArguments: true,
      flagsArePosixNoncompliant: true,
      optionArgSeparators: ["=", ":"],
    });
    assert.equal(ir.filterStrategy, "fuzzy");
    assert.equal("filterStrategy" in ir.args[0], false);
    assert.equal(ir.args[0].suggestions[0].type, "file");
    assert.equal(ir.args[0].suggestions[0].originalType, "folder");
    assert.equal(ir.args[0].suggestions[0].getQueryTerm, "/");
    assert.equal(ir.args[0].suggestions[0].argsHint, "<required> [optional]");
    assert.equal("getQueryTerm" in ir.args[0].suggestions[1], false);
    assert.deepEqual(ir.args[1].script, ["printf", "ok\n"]);
    assert.equal(ir.args[1].suggestCurrentToken, true);
    assert.equal(ir.args[1].filterStrategy, "prefix");
    assert.equal(ir.args[1].scriptTimeout, 7000);
    const color = ir.persistentOptions[0];
    assert.deepEqual(color.names, ["-c", "--color"]);
    assert.equal(color.insertValue, "--colour");
    assert.equal(color.displayName, "Color");
    assert.equal(color.separatorToAdd, ":");
    assert.equal(color.shouldAddSpace, false);
    assert.equal(color.hidden, true);
    assert.equal(color.priority, 50);
    assert.equal(color.icon, "🟡");
    assert.equal(color.isDangerous, true);
    assert.deepEqual(color.exclusiveOn, ["--no-color", "-C"]);
    assert.deepEqual(color.dependsOn, ["--config"]);
    assert.equal(color.isRepeatable, 2);
    assert.equal(color.isPersistent, true);
    assert.equal(ir.options.find((option) => option.names[0] === "--many").isRepeatable, true);
    assert.equal(ir.options.find((option) => option.names[0] === "--once").isRepeatable, false);
    const required = ir.options.find((option) => option.names[0] === "--required");
    assert.equal("separatorToAdd" in required, false);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler keeps lazy loadSpec links and maps versioned nested roots", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-links-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-links-"));
  try {
    await writeFile(
      join(srcDir, "docker.js"),
      `export default {
        name: "docker",
        subcommands: [{ name: "compose", loadSpec: "docker-compose" }, {
          name: "inline",
          loadSpec: { name: "inline", subcommands: [{ name: "up" }] },
        }],
      };\n`,
    );
    await writeFile(
      join(srcDir, "docker-compose.js"),
      `export default { name: "docker-compose", subcommands: [{ name: "up" }] };\n`,
    );
    await writeFile(
      join(srcDir, "gcloud.js"),
      `export default { name: "gcloud", subcommands: [{ name: "domains", loadSpec: "gcloud/domains" }] };\n`,
    );
    await mkdir(join(srcDir, "gcloud"), { recursive: true });
    await writeFile(
      join(srcDir, "gcloud", "domains.js"),
      `export default { name: "domains", subcommands: [{ name: "list" }] };\n`,
    );
    await mkdir(join(srcDir, "heroku"), { recursive: true });
    await writeFile(
      join(srcDir, "heroku", "index.js"),
      `export default () => ({ name: "heroku" });\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "8.0.0.js"),
      `export default { name: "heroku", subcommands: [{ name: "old" }] };\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "8.6.0.js"),
      `export default { name: "heroku", subcommands: [{ name: "new" }] };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 6);
    assert.equal(result.failed, 1);

    const docker = JSON.parse(await readFile(join(outDir, "docker.json"), "utf8"));
    assert.equal(docker.subcommands[0].loadSpec, "docker-compose");
    assert.deepEqual(
      docker.subcommands.find((item) => item.names.includes("inline"))
        .subcommands.map((item) => item.names[0]),
      ["up"],
    );

    const index = JSON.parse(await readFile(join(outDir, "index.json"), "utf8"));
    assert.equal(index.files.docker, "docker.json");
    assert.equal(index.files.heroku, "heroku/8.6.0.json");
    assert.equal(index.files.gcloud, "gcloud.json");
    assert.equal("domains" in index.files, false);
    assert.equal("gcloud/domains" in index.files, false);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("root file names are canonical and spec names are aliases", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-canonical-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-canonical-"));
  try {
    await writeFile(join(srcDir, "appwrite.js"), `export default { name: "index" };\n`);
    await writeFile(
      join(srcDir, "autojump.js"),
      `export default { name: "autojump", args: { name: "directory", isVariadic: true } };\n`,
    );
    await writeFile(join(srcDir, "j.js"), `export default { name: "autojump" };\n`);
    await writeFile(join(srcDir, "git.js"), `export default { name: "git" };\n`);
    await writeFile(join(srcDir, "hub.js"), `export default { name: "git" };\n`);

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 5);
    const index = JSON.parse(await readFile(join(outDir, "index.json"), "utf8"));
    assert.equal(index.files.appwrite, "appwrite.json");
    assert.equal(index.files.index, "appwrite.json");
    assert.equal(index.files.autojump, "autojump.json");
    assert.equal(index.files.j, "j.json");
    assert.equal(index.files.git, "git.json");
    assert.equal(index.files.hub, "hub.json");
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("object loadSpec replaces wrapper fields while retaining its command name", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-replace-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-replace-"));
  try {
    await writeFile(
      join(srcDir, "pass.js"),
      `export default {
        name: "pass",
        subcommands: [{
          name: "grep",
          description: "wrapper description",
          args: { name: "pass-name" },
          loadSpec: {
            name: "grep-target",
            description: "loaded description",
            args: [{ name: "pattern" }, { name: "file" }],
          },
        }],
      };\n`,
    );
    await writeFile(
      join(srcDir, "chezmoi.js"),
      `export default {
        name: "chezmoi",
        subcommands: [{
          name: "git",
          description: "wrapper description",
          args: { name: "source-dir" },
          loadSpec: {
            name: "git-target",
            description: "loaded description",
            args: [{ name: "command" }],
          },
        }],
      };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 2);
    const pass = JSON.parse(await readFile(join(outDir, "pass.json"), "utf8"));
    const grep = pass.subcommands[0];
    assert.deepEqual(grep.names, ["grep"]);
    assert.equal(grep.description, "loaded description");
    assert.deepEqual(grep.args.map((arg) => arg.name), ["pattern", "file"]);

    const chezmoi = JSON.parse(
      await readFile(join(outDir, "chezmoi.json"), "utf8"),
    );
    const git = chezmoi.subcommands[0];
    assert.deepEqual(git.names, ["git"]);
    assert.equal(git.description, "loaded description");
    assert.deepEqual(git.args.map((arg) => arg.name), ["command"]);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("source index keeps top-level aliases and versioned roots backward compatible", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-source-index-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-source-index-"));
  try {
    await writeFile(join(srcDir, "r.js"), `export default { name: "R" };\n`);
    await writeFile(join(srcDir, "stepzen.js"), `export default { name: "StepZen" };\n`);
    await writeFile(
      join(srcDir, "visible.js"),
      `export default { name: "visible-alias" };\n`,
    );
    await writeFile(
      join(srcDir, "hidden.js"),
      `export default { name: "hidden-alias" };\n`,
    );
    await mkdir(join(srcDir, "heroku"), { recursive: true });
    await writeFile(
      join(srcDir, "heroku", "index.js"),
      `export default () => ({ name: "heroku" });\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "8.0.0.js"),
      `export default { name: "heroku" };\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "8.6.0.js"),
      `export default { name: "heroku" };\n`,
    );
    await writeFile(
      join(srcDir, "index.json"),
      JSON.stringify({
        completions: [
          "r",
          "stepzen",
          "visible",
          "visible-alias",
          "heroku",
          "heroku/8.0.0",
          "heroku/8.6.0",
        ],
        diffVersionedCompletions: ["heroku"],
      }),
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 6);
    assert.equal(result.failed, 1);
    const index = JSON.parse(await readFile(join(outDir, "index.json"), "utf8"));
    assert.equal(index.files.r, "r.json");
    assert.equal(index.files.stepzen, "stepzen.json");
    assert.equal(index.files.visible, "visible.json");
    assert.equal(index.files["visible-alias"], "visible.json");
    assert.equal(index.files.heroku, "heroku/8.6.0.json");
    assert.equal("R" in index.files, false);
    assert.equal("StepZen" in index.files, false);
    assert.equal("hidden" in index.files, false);
    assert.equal("hidden-alias" in index.files, false);
    assert.equal("heroku/8.6.0" in index.files, false);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler keeps postProcess scripts and extracts JS hooks", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-script-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-script-"));
  try {
    await writeFile(
      join(srcDir, "fixture.js"),
      `export default {
        name: "fixture",
        generateSpec: async (tokens, exec) => ({ name: "fixture", subcommands: [{ name: "dyn" }] }),
        generateSpecCacheKey: "fixture-tree",
        args: [
          {
            name: "split",
            generators: {
              script: ["printf", "a,b,c"],
              splitOn: ",",
            },
          },
          {
            name: "post",
            generators: {
              script: ["ps", "axo", "pid,comm"],
              scriptTimeout: 9000,
              postProcess: (out) => out.split("\\n").map((line) => ({ name: line })),
            },
          },
          {
            name: "both",
            generators: {
              script: ["printf", "a\\nb"],
              splitOn: "\\n",
              postProcess: (out) => out.split("\\n").map((line) => ({ name: line })),
            },
          },
          {
            name: "custom",
            generators: {
              custom: async (tokens) => [{ name: tokens.at(-1) || "row" }],
              cache: { strategy: "stale-while-revalidate", ttl: 5000, cacheByDirectory: true, cacheKey: "env" },
            },
          },
        ],
      };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    assert.ok(result.hooks >= 4);
    const ir = JSON.parse(await readFile(join(outDir, "fixture.json"), "utf8"));
    assert.deepEqual(ir.args[0].script, ["printf", "a,b,c"]);
    assert.equal(ir.args[0].splitOn, ",");
    assert.deepEqual(ir.args[1].script, ["ps", "axo", "pid,comm"]);
    assert.equal(ir.args[1].scriptTimeout, 9000);
    assert.equal(ir.args[1].jsPostProcess, "fixture#postProcess#1");
    assert.deepEqual(ir.args[2].script, ["printf", "a\nb"]);
    assert.equal(ir.args[2].splitOn, "\n");
    assert.equal(typeof ir.args[2].jsPostProcess, "string");
    assert.equal(ir.args[3].jsCustom, "fixture#custom#3");
    assert.equal(ir.args[3].cacheKey, "env");
    assert.equal(ir.args[3].cacheByDirectory, true);
    assert.equal(ir.args[3].cacheTtl, 5000);
    assert.equal(ir.jsGenerateSpec, "fixture#generateSpec#0");
    assert.equal(ir.generateSpecCacheKey, "fixture-tree");

    const hook = await readFile(
      join(outDir, "hooks", "fixture_postProcess_1.js"),
      "utf8",
    );
    assert.match(hook, /export default/);
    assert.match(hook, /postProcess|split/);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler keeps docker/kubectl/gh dynamic scripts instead of dropping them", async () => {
  const repoDir = join(dirname(fileURLToPath(import.meta.url)), "..");
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-cli-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-cli-"));
  try {
    for (const name of ["docker.js", "kubectl.js", "gh.js"]) {
      await writeFile(
        join(srcDir, name),
        await readFile(join(repoDir, "bundle", "specs", name)),
      );
    }
    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 3);
    assert.ok(result.hooks > 50, result.hooks);
    const docker = JSON.parse(await readFile(join(outDir, "docker.json"), "utf8"));
    const exec = docker.subcommands.find((item) => item.names.includes("exec"));
    assert.ok(exec, "docker exec");
    assert.deepEqual(exec.args[0].script.slice(0, 2), ["docker", "ps"]);
    assert.match(exec.args[0].jsPostProcess, /^docker#postProcess#/);
    const kubectl = JSON.parse(await readFile(join(outDir, "kubectl.json"), "utf8"));
    assert.ok(
      JSON.stringify(kubectl).includes("jsPostProcess") ||
        JSON.stringify(kubectl).includes("jsCustom"),
      "kubectl keeps JS hooks",
    );
    const gh = JSON.parse(await readFile(join(outDir, "gh.json"), "utf8"));
    assert.match(JSON.stringify(gh), /jsPostProcess/);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler keeps isCommand, isScript, and isModule", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-cmd-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-cmd-"));
  try {
    await writeFile(
      join(srcDir, "fixture.js"),
      `export default {
        name: "fixture",
        args: [
          { name: "cmd", isCommand: true },
          { name: "script", isScript: true },
          { name: "mod", isModule: "python/" },
          { name: "plain" },
        ],
      };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    const ir = JSON.parse(await readFile(join(outDir, "fixture.json"), "utf8"));
    assert.equal(ir.args[0].isCommand, true);
    assert.equal("isScript" in ir.args[0], false);
    assert.equal(ir.args[1].isScript, true);
    assert.equal(ir.args[2].isModule, "python/");
    assert.equal("isCommand" in ir.args[3], false);
    assert.equal("isModule" in ir.args[3], false);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});
