#!/usr/bin/env node
/**
 * Compile bundled Fig JS specs into static JSON IR for the Rust engine.
 * Static walk data stays in JSON. Fig functions (postProcess / custom /
 * generateSpec / function script) are extracted as standalone hook modules
 * and referenced from the IR by id. Known Rust builtins still replace the
 * matching git/npm scripts.
 */
import { mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import {
  createFilepathsBinder,
  functionSource,
  isFilepathsHelper,
  nativeFilepathsFromHelper,
} from "./filepaths-helper.mjs";

const repoDir = join(dirname(fileURLToPath(import.meta.url)), "..");

const GIT_ALIASES_SCRIPT = [
  "git",
  "--no-optional-locks",
  "config",
  "--get-regexp",
  "^alias.",
];

function namesOf(value) {
  if (value == null || value === "") return [];
  if (Array.isArray(value)) return value.flatMap(namesOf);
  if (typeof value === "object") {
    if ("name" in value) return namesOf(value.name);
    return [];
  }
  return [String(value)];
}

function asArray(value) {
  if (value == null) return [];
  if (Array.isArray(value)) return value;
  if (typeof value === "object") return Object.values(value);
  return [];
}

function templatesOf(value) {
  if (value == null) return [];
  const list = Array.isArray(value) ? value : [value];
  const out = [];
  for (const item of list) {
    if (
      item === "filepaths" ||
      item === "folders" ||
      item === "history" ||
      item === "help"
    ) {
      out.push(item);
    }
  }
  return out;
}

function pushGens(list, value) {
  if (value == null) return;
  if (Array.isArray(value)) {
    for (const item of value) pushGens(list, item);
  } else {
    list.push(value);
  }
}

function generatorsOf(node) {
  const list = [];
  if (!node || typeof node !== "object") return list;
  pushGens(list, node.generators);
  pushGens(list, node.generator);
  return list;
}

// `loadSpec` is a runtime escape hatch in Fig specs.  Native IR can keep a
// string reference and resolve it from the bundled JSON at load time, but it
// must never serialize executable JavaScript.  Inline objects are safe because
// they are already plain data; functions and other dynamic values are omitted.
async function applyLoadSpec(raw, ctx, out) {
  if (!raw || typeof raw !== "object" || !out) return;
  if (typeof raw.loadSpec === "string" && raw.loadSpec.trim()) {
    out.loadSpec = raw.loadSpec.trim();
    return;
  }
  if (typeof raw.loadSpec === "function" && ctx?.hooks) {
    const hookId = extractHook(ctx.hooks, "loadSpec", raw.loadSpec);
    if (hookId) out.jsLoadSpec = hookId;
    return;
  }
  if (
    raw.loadSpec &&
    typeof raw.loadSpec === "object" &&
    !Array.isArray(raw.loadSpec)
  ) {
    const loaded = await convertNode(raw.loadSpec, ctx);
    if (loaded) out.loadSpec = loaded;
  }
}

function scriptOf(gen) {
  if (!gen || typeof gen !== "object" || typeof gen === "function") return [];
  const script = gen.script;
  if (typeof script === "string" && script.trim()) {
    return ["sh", "-c", script];
  }
  if (
    Array.isArray(script) &&
    script.length > 0 &&
    script.every((part) => typeof part === "string")
  ) {
    return script;
  }
  if (
    script &&
    typeof script === "object" &&
    typeof script.command === "string" &&
    script.command.trim()
  ) {
    const args = Array.isArray(script.args)
      ? script.args.filter((part) => typeof part === "string")
      : [];
    return [script.command, ...args];
  }
  return [];
}

// Keep the numeric millisecond timeout attached to a static script/builtin.
// Fig's runtime compares this value with the user setting and any
// ExecuteCommand timeout using Math.max; invalid values are ignored just as
// a missing generator timeout is.
function splitOnOf(gen) {
  if (!gen || typeof gen !== "object" || typeof gen === "function") {
    return undefined;
  }
  return typeof gen.splitOn === "string" ? gen.splitOn : undefined;
}

export function createHookBag(specId) {
  return { specId, next: 0, files: new Map() };
}

export function hookFileName(id) {
  return `${String(id).replace(/[^A-Za-z0-9._-]+/g, "_")}.js`;
}

function passCtx(ctx, extra = {}) {
  return {
    rootName: extra.rootName ?? ctx.rootName,
    nodeNames: extra.nodeNames ?? ctx.nodeNames,
    hooks: ctx.hooks,
    source: ctx.source,
    binder: ctx.binder,
  };
}

function extractHook(hooks, kind, fn) {
  if (!hooks || typeof fn !== "function") return undefined;
  const src = functionSource(fn);
  if (!src) return undefined;
  const id = `${hooks.specId}#${kind}#${hooks.next++}`;
  hooks.files.set(id, `export default ${src};\n`);
  return id;
}

function cacheFieldsOf(gen) {
  const cache = gen && typeof gen === "object" ? gen.cache : null;
  if (!cache || typeof cache !== "object") return {};
  const out = {};
  if (typeof cache.cacheKey === "string" && cache.cacheKey) {
    out.cacheKey = cache.cacheKey;
  }
  if (typeof cache.cacheByDirectory === "boolean") {
    out.cacheByDirectory = cache.cacheByDirectory;
  }
  if (typeof cache.ttl === "number" && Number.isFinite(cache.ttl)) {
    out.cacheTtl = Math.trunc(cache.ttl);
  }
  if (cache.strategy === "max-age" || cache.strategy === "stale-while-revalidate") {
    out.cacheStrategy = cache.strategy;
  }
  return out;
}

function triggerOf(gen, ctx = {}) {
  if (!gen || typeof gen !== "object") return undefined;
  const trigger = gen.trigger;
  if (trigger == null) return undefined;
  if (typeof trigger === "string") return { on: "string", string: trigger };
  if (typeof trigger === "function" && ctx.hooks) {
    const hookId = extractHook(ctx.hooks, "trigger", trigger);
    return hookId ? { on: "function", jsTrigger: hookId } : undefined;
  }
  if (typeof trigger !== "object") return undefined;
  if (trigger.on === "threshold") {
    const length =
      typeof trigger.length === "number" && Number.isFinite(trigger.length)
        ? Math.trunc(trigger.length)
        : 0;
    return { on: "threshold", length };
  }
  if (trigger.on === "match") {
    const strings = Array.isArray(trigger.string)
      ? trigger.string.filter((item) => typeof item === "string")
      : typeof trigger.string === "string"
        ? [trigger.string]
        : [];
    return { on: "match", string: strings };
  }
  return { on: "change" };
}

function debounceMsOf(arg) {
  if (!arg || typeof arg !== "object") return undefined;
  if (arg.debounce === true) return 200;
  if (typeof arg.debounce === "number" && Number.isFinite(arg.debounce) && arg.debounce > 0) {
    return Math.trunc(arg.debounce);
  }
  return undefined;
}

function scriptTimeoutOf(gen) {
  if (!gen || typeof gen !== "object" || typeof gen === "function") {
    return undefined;
  }
  const values = [gen.scriptTimeout];
  if (gen.script && typeof gen.script === "object" && !Array.isArray(gen.script)) {
    values.push(gen.script.timeout);
  }
  const numeric = values
    .filter((value) => typeof value === "number" && Number.isFinite(value))
    .map((value) => Math.trunc(value));
  return numeric.length ? Math.max(...numeric) : undefined;
}

function normalizeArgName(value) {
  const names = namesOf(value);
  return names.length ? names[0].toLowerCase() : "";
}

// Map the same argv that the Fig generator used to a native Rust generator.
// Keeping this based on argv is important: `remote` and `branch` arguments in
// push/pull/fetch intentionally have different data sources.
function inferBuiltinFromScript(rootName, _nodeNames, _argName, script) {
  const root = String(rootName ?? "").toLowerCase();
  if (root !== "git" || !Array.isArray(script) || script.length === 0)
    return null;
  const argv = script.map((part) => String(part).toLowerCase());
  if (argv.includes("status") && argv.includes("--short"))
    return "git-changed-files";
  if (argv.includes("diff") && argv.includes("--name-only"))
    return "git-changed-files";
  if (
    argv.includes("remote") &&
    (argv.includes("-v") || argv.includes("--verbose"))
  ) {
    return "git-remotes";
  }
  if (argv.includes("remote")) return "git-remotes";
  if (argv.includes("tag") && argv.includes("--list")) return "git-tags";
  if (argv.includes("stash") && argv.includes("list")) return "git-stashes";
  if (argv.includes("branch")) return "git-branches";
  if (argv.includes("log") || argv.includes("rev-list")) return "git-commits";
  return null;
}

function inferScriptBuiltin(rootName, script) {
  const root = String(rootName ?? "").toLowerCase();
  if (
    root === "git" &&
    Array.isArray(script) &&
    script.length === GIT_ALIASES_SCRIPT.length &&
    script.every((part, index) => part === GIT_ALIASES_SCRIPT[index])
  ) {
    return "git-aliases";
  }
  return null;
}

function suggestionSeedsOf(value, ctx = {}) {
  if (value == null) return [];
  const list = Array.isArray(value) ? value : [value];
  return list
    .map((item) => {
      const names = namesOf(item);
      if (!names.length) return null;
      const seed = { names };
      if (item && typeof item === "object" && item.description) {
        seed.description = String(item.description);
      }
      const argsHint = argsHintOf(item && typeof item === "object" ? item.args : undefined);
      if (argsHint) seed.argsHint = argsHint;
      copySuggestionMetadata(item, seed, ctx);
      return seed;
    })
    .filter(Boolean);
}

function argsHintOf(value) {
  if (value == null) return "";
  const args = Array.isArray(value) ? value : [value];
  return args
    .filter((arg) => arg && typeof arg === "object" && arg.name != null)
    .map((arg) => {
      const rawName = Array.isArray(arg.name) ? arg.name[0] : arg.name;
      if (rawName == null || rawName === "") return "";
      const name = String(rawName);
      const base = arg.isVariadic ? `${name}...` : name;
      return arg.isOptional ? `[${base}]` : `<${base}>`;
    })
    .filter(Boolean)
    .join(" ");
}

function filterStrategyOf(raw) {
  const strategy = raw?.filterStrategy;
  return ["prefix", "fuzzy", "default"].includes(strategy) ? strategy : undefined;
}

// Keep the metadata that affects acceptance and ordering in the static IR.
// Runtime generators are intentionally still omitted, but a static suggestion
// must behave like the same suggestion did in the WebView implementation.
function copySuggestionMetadata(raw, out, ctx = {}) {
  if (!raw || typeof raw !== "object") return;
  if (raw.insertValue != null) out.insertValue = String(raw.insertValue);
  if (raw.displayName != null) out.displayName = String(raw.displayName);
  if (raw.separatorToAdd != null)
    out.separatorToAdd = String(raw.separatorToAdd);
  // A static suggestion may override the default kind assigned by the
  // surrounding spec (for example a folder/file row in an argument's
  // `suggestions` array).  Keep both type values because auto-execute rows
  // use originalType to recover the underlying row type.
  if (typeof raw.type === "string" && raw.type) out.type = raw.type;
  if (typeof raw.originalType === "string" && raw.originalType) {
    out.originalType = raw.originalType;
  }
  if (typeof raw.getQueryTerm === "string") out.getQueryTerm = raw.getQueryTerm;
  else if (typeof raw.getQueryTerm === "function" && ctx.hooks) {
    const hookId = extractHook(ctx.hooks, "getQueryTerm", raw.getQueryTerm);
    if (hookId) out.jsGetQueryTerm = hookId;
  }
  if (typeof raw.getQueryTerm !== "string" && raw.getQueryTerm == null) {
    for (const generator of generatorsOf(raw)) {
      if (isFilepathsHelper(generator)) {
        out.getQueryTerm = "/";
        break;
      }
      if (!generator || typeof generator !== "object") continue;
      if (typeof generator.getQueryTerm === "string") {
        out.getQueryTerm = generator.getQueryTerm;
        break;
      }
      if (typeof generator.getQueryTerm === "function" && ctx.hooks) {
        const hookId = extractHook(
          ctx.hooks,
          "getQueryTerm",
          generator.getQueryTerm,
        );
        if (hookId) {
          out.jsGetQueryTerm = hookId;
          break;
        }
      }
    }
  }
  if (typeof raw.shouldAddSpace === "boolean")
    out.shouldAddSpace = raw.shouldAddSpace;
  if (typeof raw.hidden === "boolean") out.hidden = raw.hidden;
  if (typeof raw.priority === "number" && Number.isFinite(raw.priority)) {
    const priority = Math.trunc(raw.priority);
    out.priority = priority === 0 ? 50 : Math.max(0, Math.min(100, priority));
  }
  if (
    raw.icon != null &&
    (typeof raw.icon === "string" || typeof raw.icon === "number")
  ) {
    out.icon = String(raw.icon);
  }
  if (typeof raw.isDangerous === "boolean") out.isDangerous = raw.isDangerous;
}

function separatorToAdd(raw) {
  if (!raw || typeof raw !== "object") return undefined;
  // A separator only belongs on options whose first argument is mandatory;
  // this mirrors suggestions/index.ts and avoids inserting `=` for optional
  // values.  Leave boolean `requiresSeparator: true` to the native lookup so
  // it can use parserDirectives.optionArgSeparators instead of baking in `=`.
  const firstArg = Array.isArray(raw.args) ? raw.args[0] : raw.args;
  if (firstArg && typeof firstArg === "object" && firstArg.isOptional)
    return undefined;
  if (typeof raw.requiresSeparator === "string") return raw.requiresSeparator;
  if (raw.requiresEquals) return "=";
  return undefined;
}

// Option state is consumed by the parser before suggestions are filtered.
// Keep the JSON values (rather than collapsing them to booleans) so the
// native engine can preserve Fig's `isRepeatable: true` (unlimited) versus a
// numeric repetition limit.
function copyOptionStateMetadata(raw, out) {
  if (!raw || typeof raw !== "object") return;
  const exclusiveOn = namesOf(raw.exclusiveOn);
  if (exclusiveOn.length) out.exclusiveOn = exclusiveOn;
  const dependsOn = namesOf(raw.dependsOn);
  if (dependsOn.length) out.dependsOn = dependsOn;
  if (typeof raw.isRepeatable === "boolean") {
    out.isRepeatable = raw.isRepeatable;
  } else if (
    typeof raw.isRepeatable === "number" &&
    Number.isFinite(raw.isRepeatable)
  ) {
    out.isRepeatable = raw.isRepeatable;
  }
  if (raw.isPersistent === true) out.isPersistent = true;
}

function parserDirectivesOf(raw, ctx = {}) {
  const directives = raw?.parserDirectives;
  if (!directives || typeof directives !== "object") return undefined;
  const out = {};
  if (typeof directives.optionsMustPrecedeArguments === "boolean") {
    out.optionsMustPrecedeArguments = directives.optionsMustPrecedeArguments;
  }
  if (typeof directives.flagsArePosixNoncompliant === "boolean") {
    out.flagsArePosixNoncompliant = directives.flagsArePosixNoncompliant;
  }
  if (Array.isArray(directives.optionArgSeparators)) {
    const separators = directives.optionArgSeparators.filter(
      (separator) => typeof separator === "string",
    );
    out.optionArgSeparators = separators;
  }
  if (typeof directives.alias === "string" && directives.alias) {
    out.alias = directives.alias;
  } else if (typeof directives.alias === "function" && ctx?.hooks) {
    const hookId = extractHook(ctx.hooks, "alias", directives.alias);
    if (hookId) out.jsAlias = hookId;
  }
  return Object.keys(out).length ? out : undefined;
}

async function convertArg(raw, ctx) {
  const arg = raw && typeof raw === "object" ? raw : {};
  let templates = templatesOf(arg.template);
  let script = [];
  let splitOn;
  let scriptTimeout;
  let jsPostProcess;
  let jsCustom;
  let jsScript;
  let cacheFields = {};
  const nativeBuiltins = [];
  const generators = [];
  const argName = normalizeArgName(arg.name);
  for (const gen of generatorsOf(arg)) {
    const converted = await convertGenerator(gen, ctx, argName);
    if (converted) generators.push(converted);
    const fromScript = converted?.script ?? [];
    const builtin = converted?.builtin;
    if (builtin) nativeBuiltins.push(builtin);
    else if (!script.length && fromScript.length) {
      script = fromScript;
      if (converted.splitOn !== undefined) splitOn = converted.splitOn;
    }
    if (converted?.jsPostProcess && !jsPostProcess) {
      jsPostProcess = converted.jsPostProcess;
    }
    if (converted?.jsCustom && !jsCustom) jsCustom = converted.jsCustom;
    if (converted?.jsScript && !jsScript) jsScript = converted.jsScript;
    if (converted?.scriptTimeout !== undefined) {
      scriptTimeout =
        scriptTimeout === undefined
          ? converted.scriptTimeout
          : Math.max(scriptTimeout, converted.scriptTimeout);
    }
    const nextCache = cacheFieldsOf(gen);
    if (Object.keys(nextCache).length) cacheFields = { ...cacheFields, ...nextCache };
    if (converted?.templates?.length) {
      templates = [...new Set([...templates, ...converted.templates])];
    }
  }

  const scriptBuiltin = inferScriptBuiltin(ctx.rootName, script);
  if (scriptBuiltin) nativeBuiltins.push(scriptBuiltin);
  const uniqueBuiltins = [...new Set(nativeBuiltins)];
  // A recognized git/npm argv is replaced by the native builtin that
  // implements the same script. postProcess/custom stay: they are not the
  // script. Unrecognized scripts are left as argv + hooks, matching Fig.
  if (scriptBuiltin) {
    script = [];
    splitOn = undefined;
  }

  const out = {};
  if (arg.name != null && arg.name !== "") {
    const name = Array.isArray(arg.name) ? arg.name[0] : arg.name;
    if (name) out.name = String(name);
  }
  if (arg.description) out.description = String(arg.description);
  copySuggestionMetadata(arg, out, ctx);
  // Preserve an explicit false separately from an omitted value. The native
  // lookup uses this as an argument-level override of the global setting.
  if (typeof arg.suggestCurrentToken === "boolean") {
    out.suggestCurrentToken = arg.suggestCurrentToken;
  }
  if (typeof arg.optionsCanBreakVariadicArg === "boolean") {
    out.optionsCanBreakVariadicArg = arg.optionsCanBreakVariadicArg;
  }
  const filterStrategy = filterStrategyOf(arg);
  if (filterStrategy) out.filterStrategy = filterStrategy;
  await applyLoadSpec(arg, ctx, out);
  const argDirectives = parserDirectivesOf(arg, ctx);
  if (argDirectives) out.parserDirectives = argDirectives;
  const debounceMs = debounceMsOf(arg);
  if (debounceMs !== undefined) out.debounceMs = debounceMs;
  if (templates.length) out.templates = templates;
  if (script.length) out.script = script;
  if (splitOn !== undefined) out.splitOn = splitOn;
  if (scriptTimeout !== undefined) out.scriptTimeout = scriptTimeout;
  if (jsPostProcess) out.jsPostProcess = jsPostProcess;
  if (jsCustom) out.jsCustom = jsCustom;
  if (jsScript) out.jsScript = jsScript;
  if (cacheFields.cacheKey) out.cacheKey = cacheFields.cacheKey;
  if (typeof cacheFields.cacheByDirectory === "boolean") {
    out.cacheByDirectory = cacheFields.cacheByDirectory;
  }
  if (cacheFields.cacheTtl !== undefined) out.cacheTtl = cacheFields.cacheTtl;
  if (cacheFields.cacheStrategy) out.cacheStrategy = cacheFields.cacheStrategy;
  if (uniqueBuiltins.length === 1) out.builtin = uniqueBuiltins[0];
  else if (uniqueBuiltins.length > 1) out.builtins = uniqueBuiltins;
  if (generators.length) out.generators = generators;
  const suggestions = suggestionSeedsOf(arg.suggestions, ctx);
  if (arg.isOptional) out.isOptional = true;
  if (arg.isVariadic) out.isVariadic = true;
  if (arg.isCommand) out.isCommand = true;
  if (arg.isScript) out.isScript = true;
  if (typeof arg.isModule === "string" && arg.isModule) {
    out.isModule = arg.isModule;
  }
  if (suggestions.length) out.suggestions = suggestions;
  if (!out.getQueryTerm) {
    const queryTerm = generators.find((generator) => generator.getQueryTerm)?.getQueryTerm;
    if (queryTerm) out.getQueryTerm = queryTerm;
  }
  return out;
}

async function convertGenerator(gen, ctx, argName) {
  if (!gen || (typeof gen !== "object" && typeof gen !== "function")) return null;
  if (isFilepathsHelper(gen)) {
    const hints = [argName, ...(ctx.nodeNames ?? [])];
    const literal = ctx.binder?.take(hints) ?? null;
    return nativeFilepathsFromHelper(gen, literal);
  }
  if (typeof gen === "function") return null;
  const out = {};
  const templates = templatesOf(gen.template);
  if (templates.length) out.templates = templates;
  const fromScript = scriptOf(gen);
  const builtin = inferBuiltinFromScript(
    ctx.rootName,
    ctx.nodeNames ?? [],
    argName,
    fromScript,
  );
  if (builtin) out.builtin = builtin;
  else if (fromScript.length) out.script = fromScript;
  const splitOn = splitOnOf(gen);
  if (splitOn !== undefined && !builtin) out.splitOn = splitOn;
  const scriptTimeout = scriptTimeoutOf(gen);
  if (scriptTimeout !== undefined) out.scriptTimeout = scriptTimeout;
  if (typeof gen.script === "function" && ctx.hooks) {
    const hookId = extractHook(ctx.hooks, "script", gen.script);
    if (hookId) out.jsScript = hookId;
  }
  if (typeof gen.postProcess === "function" && ctx.hooks) {
    const hookId = extractHook(ctx.hooks, "postProcess", gen.postProcess);
    if (hookId) out.jsPostProcess = hookId;
  }
  if (typeof gen.custom === "function" && ctx.hooks) {
    const hookId = extractHook(ctx.hooks, "custom", gen.custom);
    if (hookId) out.jsCustom = hookId;
  }
  if (typeof gen.filterTemplateSuggestions === "function" && ctx.hooks) {
    const hookId = extractHook(
      ctx.hooks,
      "filterTemplateSuggestions",
      gen.filterTemplateSuggestions,
    );
    if (hookId) out.jsFilterTemplateSuggestions = hookId;
  }
  if (typeof gen.getQueryTerm === "string") out.getQueryTerm = gen.getQueryTerm;
  else if (typeof gen.getQueryTerm === "function" && ctx.hooks) {
    const hookId = extractHook(ctx.hooks, "getQueryTerm", gen.getQueryTerm);
    if (hookId) out.jsGetQueryTerm = hookId;
  }
  Object.assign(out, cacheFieldsOf(gen));
  const trigger = triggerOf(gen, ctx);
  if (trigger) out.trigger = trigger;
  return Object.keys(out).length ? out : null;
}

async function convertOption(raw, ctx = {}) {
  if (!raw || typeof raw !== "object") return null;
  const names = namesOf(raw.name);
  if (!names.length) return null;
  const out = { names };
  if (raw.description) out.description = String(raw.description);
  copySuggestionMetadata(raw, out, ctx);
  copyOptionStateMetadata(raw, out);
  await applyLoadSpec(raw, ctx, out);
  const separator = separatorToAdd(raw);
  if (separator !== undefined && out.separatorToAdd === undefined) {
    out.separatorToAdd = separator;
  }
  const argsRaw = Array.isArray(raw.args)
    ? raw.args
    : raw.args
      ? [raw.args]
      : [];
  const args = [];
  for (const arg of argsRaw) {
    const converted = await convertArg(
      arg,
      passCtx(ctx, {
        rootName: ctx.rootName ?? "",
        nodeNames: names,
      }),
    );
    if (Object.keys(converted).length > 0) args.push(converted);
  }
  if (args.length) out.args = args;
  return out;
}

async function convertNode(raw, ctx) {
  if (!raw || typeof raw !== "object") return null;
  const names = namesOf(raw.name);
  if (!names.length) return null;
  const rootName = ctx.rootName ?? names[0];
  const childCtx = passCtx(ctx, { rootName, nodeNames: names });
  const out = { names };
  if (raw.description) out.description = String(raw.description);
  copySuggestionMetadata(raw, out, ctx);
  if (typeof raw.requiresSubcommand === "boolean") {
    out.requiresSubcommand = raw.requiresSubcommand;
  }
  const filterStrategy = filterStrategyOf(raw);
  if (filterStrategy) out.filterStrategy = filterStrategy;
  const parserDirectives = parserDirectivesOf(raw, ctx);
  if (parserDirectives) out.parserDirectives = parserDirectives;

  const additionalSuggestions = suggestionSeedsOf(
    raw.additionalSuggestions,
    ctx,
  );
  if (additionalSuggestions.length)
    out.additionalSuggestions = additionalSuggestions;

  if (ctx.hooks && typeof raw.generateSpec === "function") {
    const jsGenerateSpec = extractHook(ctx.hooks, "generateSpec", raw.generateSpec);
    if (jsGenerateSpec) out.jsGenerateSpec = jsGenerateSpec;
    if (typeof raw.generateSpecCacheKey === "string" && raw.generateSpecCacheKey) {
      out.generateSpecCacheKey = raw.generateSpecCacheKey;
    }
  }

  const subcommands = [];
  for (const item of asArray(raw.subcommands)) {
    const converted = await convertNode(item, passCtx(ctx, { rootName }));
    if (converted) subcommands.push(converted);
  }
  if (subcommands.length) out.subcommands = subcommands;

  const options = [];
  for (const option of asArray(raw.options)) {
    const converted = await convertOption(option, childCtx);
    if (converted) options.push(converted);
  }
  const persistentOptions = options.filter((option) => option.isPersistent === true);
  const regularOptions = options.filter((option) => option.isPersistent !== true);
  if (regularOptions.length) out.options = regularOptions;
  if (persistentOptions.length) out.persistentOptions = persistentOptions;

  const argsRaw = Array.isArray(raw.args)
    ? raw.args
    : raw.args
      ? [raw.args]
      : [];
  let args = [];
  for (const arg of argsRaw) {
    const converted = await convertArg(arg, childCtx);
    if (Object.keys(converted).length > 0) args.push(converted);
  }

  // Spec-level generators (not under args) attach to the first argument so
  // the native runner can execute them the same way as arg-level generators.
  if (args.every((arg) => !arg.script && !arg.jsCustom && !arg.jsScript)) {
    for (const gen of generatorsOf(raw)) {
      if (isFilepathsHelper(gen)) {
        const converted = await convertGenerator(gen, ctx, "");
        if (converted) {
          if (args.length === 0) args = [{}];
          args[0].templates = [
            ...new Set([...(args[0].templates ?? []), ...(converted.templates ?? [])]),
          ];
          args[0].generators = [...(args[0].generators ?? []), converted];
          if (converted.getQueryTerm && !args[0].getQueryTerm) {
            args[0].getQueryTerm = converted.getQueryTerm;
          }
        }
        break;
      }
      const script = scriptOf(gen);
      const jsScript =
        typeof gen?.script === "function"
          ? extractHook(ctx.hooks, "script", gen.script)
          : undefined;
      const jsCustom =
        typeof gen?.custom === "function"
          ? extractHook(ctx.hooks, "custom", gen.custom)
          : undefined;
      if (!script.length && !jsScript && !jsCustom) continue;
      if (args.length === 0) args = [{}];
      if (script.length && !args[0].script) {
        args[0].script = script;
        const splitOn = splitOnOf(gen);
        if (splitOn !== undefined) args[0].splitOn = splitOn;
        if (typeof gen.postProcess === "function") {
          const jsPostProcess = extractHook(
            ctx.hooks,
            "postProcess",
            gen.postProcess,
          );
          if (jsPostProcess) args[0].jsPostProcess = jsPostProcess;
        }
      }
      if (jsScript) args[0].jsScript = jsScript;
      if (jsCustom) args[0].jsCustom = jsCustom;
      const scriptTimeout = scriptTimeoutOf(gen);
      if (scriptTimeout !== undefined) args[0].scriptTimeout = scriptTimeout;
      Object.assign(args[0], cacheFieldsOf(gen));
      break;
    }
  }

  if (args.length) out.args = args;

  // A plain object loadSpec follows the parser's replacement semantics: the
  // loaded object replaces the wrapper's fields, while the wrapper names are
  // retained so aliases such as `docker compose` still match the typed tree.
  // String references stay lazy and are resolved by the Rust Registry,
  // avoiding a large copy of gcloud's child specs in gcloud.json.
  if (
    raw.loadSpec &&
    typeof raw.loadSpec === "object" &&
    !Array.isArray(raw.loadSpec)
  ) {
    const loaded = await convertNode(raw.loadSpec, passCtx(ctx, { rootName }));
    if (loaded) return replaceNodeWithLoaded(out, loaded);
  } else {
    await applyLoadSpec(raw, passCtx(ctx, { rootName }), out);
  }
  return out;
}

function replaceNodeWithLoaded(wrapper, loaded) {
  return {
    ...loaded,
    // The parser replaces the wrapper object, but the native tree still needs
    // the spelling used by its parent (for example `compose` vs
    // `docker-compose`) to locate the loaded node.
    names: wrapper.names?.length ? wrapper.names : loaded.names ?? [],
  };
}

async function walkJs(dir, base = dir, acc = []) {
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch {
    return acc;
  }
  for (const entry of entries) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "icons") continue;
      await walkJs(full, base, acc);
    } else if (
      entry.isFile() &&
      entry.name.endsWith(".js") &&
      (entry.name !== "index.js" || dir !== base)
    ) {
      acc.push(relative(base, full));
    }
  }
  return acc;
}

async function readSourceIndex(srcDir) {
  try {
    const index = JSON.parse(await readFile(join(srcDir, "index.json"), "utf8"));
    const list = (value) =>
      Array.isArray(value)
        ? new Set(value.filter((item) => typeof item === "string" && item))
        : new Set();
    return {
      completions: list(index.completions),
      diffVersionedCompletions: list(index.diffVersionedCompletions),
    };
  } catch {
    // Small compiler fixtures and older source trees may not ship an index.
    // In that case preserve the compiler's historical file-tree discovery.
    return null;
  }
}

function sourceCommandAllowed(sourceIndex, name) {
  if (!sourceIndex) return true;
  return (
    sourceIndex.completions.has(name) ||
    sourceIndex.diffVersionedCompletions.has(name)
  );
}

function sourceVersionedRoot(sourceIndex, directory) {
  return (
    !sourceIndex || sourceIndex.diffVersionedCompletions.has(directory)
  );
}

export async function compileSpecsIr({
  srcDir = join(repoDir, "bundle", "specs"),
  outDir = join(repoDir, "bundle", "specs-ir"),
} = {}) {
  const files = await walkJs(srcDir);
  const sourceIndex = await readSourceIndex(srcDir);
  await rm(outDir, { force: true, recursive: true });
  await mkdir(outDir, { recursive: true });

  const nestedIndexDirs = new Set(
    files
      .filter((rel) => rel.endsWith("/index.js"))
      .map((rel) => dirname(rel)),
  );
  const compiledSpecs = [];
  let compiled = 0;
  let failed = 0;
  let hooksWritten = 0;
  const hooksDir = join(outDir, "hooks");
  await mkdir(hooksDir, { recursive: true });

  for (const rel of files) {
    const src = join(srcDir, rel);
    try {
      const specId = rel.replace(/\.js$/, "").replaceAll("\\", "/");
      const hooks = createHookBag(specId);
      const source = await readFile(src, "utf8");
      const binder = createFilepathsBinder(source);
      const mod = await import(pathToFileURL(src).href);
      const spec = await convertNode(mod.default ?? mod, { hooks, source, binder });
      if (!spec) {
        failed += 1;
        continue;
      }
      const destRel = rel.replace(/\.js$/, ".json");
      const dest = join(outDir, destRel);
      await mkdir(dirname(dest), { recursive: true });
      await writeFile(dest, `${JSON.stringify(spec)}\n`);
      for (const [id, source] of hooks.files) {
        await writeFile(join(hooksDir, hookFileName(id)), source);
        hooksWritten += 1;
      }
      compiled += 1;
      compiledSpecs.push({ rel, destRel, spec });
    } catch (err) {
      failed += 1;
      process.stderr.write(`warning: skip ${rel}: ${err.message}\n`);
    }
    if ((compiled + failed) % 100 === 0 || compiled + failed === files.length) {
      process.stdout.write(
        `Compiled ${compiled}/${files.length} specs (${failed} skipped)\n`,
      );
    }
  }

  const commandFiles = new Map();
  const candidateFor = (name, candidate) => {
    if (!name || !candidate) return;
    const current = commandFiles.get(name);
    if (!current || compareFileCandidates(candidate, current) > 0) {
      commandFiles.set(name, candidate);
    }
  };
  for (const item of compiledSpecs) {
    const rel = item.rel.replaceAll("\\", "/");
    const slash = rel.lastIndexOf("/");
    const directory = slash === -1 ? "" : rel.slice(0, slash);
    const basename = rel.slice(slash + 1);
    if (!directory) {
      // The file name is the canonical root command.  Spec-declared names are
      // aliases only; giving them a lower priority prevents a colliding alias
      // (for example `j.js` naming itself `autojump`) from shadowing the
      // command's own file.
      const canonical = rel.slice(0, -3);
      if (sourceCommandAllowed(sourceIndex, canonical)) {
        candidateFor(canonical, { destRel: item.destRel, priority: 6 });
      }
      for (const name of item.spec.names) {
        if (sourceCommandAllowed(sourceIndex, name)) {
          candidateFor(name, { destRel: item.destRel, priority: 5 });
        }
      }
      continue;
    }
    if (!nestedIndexDirs.has(directory)) continue;
    if (!sourceVersionedRoot(sourceIndex, directory)) continue;
    if (basename === "index.js") {
      // A statically exported nested index is authoritative.  Dynamic
      // version selectors are skipped above and therefore fall through to
      // the deterministic highest version candidate below.
      if (sourceCommandAllowed(sourceIndex, directory)) {
        candidateFor(directory, { destRel: item.destRel, priority: 4 });
      }
      for (const name of item.spec.names) {
        if (sourceCommandAllowed(sourceIndex, name)) {
          candidateFor(name, { destRel: item.destRel, priority: 3 });
        }
      }
      continue;
    }
    const version = parseVersionFilename(basename);
    if (!version) continue;
    if (sourceCommandAllowed(sourceIndex, directory)) {
      candidateFor(directory, { destRel: item.destRel, priority: 2, version });
    }
    for (const name of item.spec.names) {
      if (sourceCommandAllowed(sourceIndex, name)) {
        candidateFor(name, { destRel: item.destRel, priority: 1, version });
      }
    }
  }

  const unique = [...commandFiles.keys()].sort();
  await writeFile(
    join(outDir, "index.json"),
    `${JSON.stringify({
      completions: unique,
      // New readers use this map to resolve command aliases without exposing
      // nested implementation files (notably gcloud/*) as top-level commands.
      // Readers predating this field continue to use relative file names.
      files: Object.fromEntries(
        [...commandFiles.entries()]
          .sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
          .map(([name, candidate]) => [name, candidate.destRel]),
      ),
    })}\n`,
  );
  process.stdout.write(
    `Wrote ${compiled} IR specs (${unique.length} names, ${hooksWritten} hooks) to ${outDir}\n`,
  );
  return { compiled, failed, names: unique.length, hooks: hooksWritten };
}

function parseVersionFilename(filename) {
  const stem = filename.replace(/\.js$/, "");
  const match = stem.match(/^v?(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-([0-9A-Za-z.-]+))?$/);
  if (!match) return null;
  return {
    numbers: [match[1], match[2] ?? "0", match[3] ?? "0"].map(Number),
    prerelease: match[4] ?? "",
    raw: stem,
  };
}

function compareVersions(left, right) {
  for (let index = 0; index < left.numbers.length; index += 1) {
    if (left.numbers[index] !== right.numbers[index]) {
      return left.numbers[index] > right.numbers[index] ? 1 : -1;
    }
  }
  if (!left.prerelease && right.prerelease) return 1;
  if (left.prerelease && !right.prerelease) return -1;
  if (left.prerelease && right.prerelease) {
    const leftParts = left.prerelease.split(".");
    const rightParts = right.prerelease.split(".");
    for (let index = 0; index < Math.max(leftParts.length, rightParts.length); index += 1) {
      if (index >= leftParts.length) return -1;
      if (index >= rightParts.length) return 1;
      const leftPart = leftParts[index];
      const rightPart = rightParts[index];
      if (leftPart === rightPart) continue;
      const leftNumeric = /^\d+$/.test(leftPart);
      const rightNumeric = /^\d+$/.test(rightPart);
      if (leftNumeric && rightNumeric) {
        return Number(leftPart) > Number(rightPart) ? 1 : -1;
      }
      if (leftNumeric !== rightNumeric) return leftNumeric ? -1 : 1;
      return leftPart < rightPart ? -1 : 1;
    }
  }
  return left.raw < right.raw ? -1 : left.raw > right.raw ? 1 : 0;
}

function compareFileCandidates(left, right) {
  if (left.priority !== right.priority) return left.priority > right.priority ? 1 : -1;
  if (left.version && right.version) return compareVersions(left.version, right.version);
  return left.destRel.localeCompare(right.destRel) > 0 ? 1 : -1;
}

const isMain =
  process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;

if (isMain) {
  const srcDir = process.env.EC_SPECS_SRC || join(repoDir, "bundle", "specs");
  const outDir = process.env.EC_SPECS_IR || join(repoDir, "bundle", "specs-ir");
  await compileSpecsIr({ srcDir, outDir });
}
