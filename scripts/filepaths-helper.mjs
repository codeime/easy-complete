/**
 * Compile-time rewrite of Fig `filepaths()` / `folders()` helpers.
 * Option literals in the spec source are authoritative. The live helper is
 * only probed for values the source cannot name (priority, exec cwd).
 */

export function functionSource(fn) {
  if (typeof fn !== "function") return null;
  let src = Function.prototype.toString.call(fn);
  if (!src || src.includes("[native code]")) return null;
  if (
    !src.startsWith("function") &&
    !src.startsWith("async function") &&
    !src.includes("=>")
  ) {
    src = src.replace(
      /^(async\s+)?[A-Za-z_$][\w$]*/,
      (_, asyncKw) => `${asyncKw || ""}function`,
    );
  }
  return src;
}

export function isFilepathsHelper(gen) {
  if (!gen || (typeof gen !== "object" && typeof gen !== "function")) {
    return false;
  }
  if (typeof gen.custom !== "function") return false;
  if (templatesOf(gen.template).length) return false;
  const src = functionSource(gen.custom) || "";
  return src.includes("-1ApL") && src.includes(".DS_Store");
}

function templatesOf(value) {
  if (value == null) return [];
  const list = Array.isArray(value) ? value : [value];
  return list.filter(
    (item) =>
      item === "filepaths" ||
      item === "folders" ||
      item === "history" ||
      item === "help",
  );
}

const FILEPATHS_PROBE_EXTENSIONS = [
  "M",
  "bar.py",
  "c",
  "cc",
  "cjs",
  "class",
  "config.js",
  "cpp",
  "csproj",
  "csv",
  "cts",
  "dart",
  "db",
  "deb",
  "dll",
  "f",
  "f90",
  "f95",
  "go",
  "jar",
  "java",
  "js",
  "json",
  "jsx",
  "m",
  "md",
  "mjs",
  "mjsx",
  "mm",
  "mod",
  "mts",
  "mtsx",
  "nupkg",
  "pdf",
  "pem",
  "pfx",
  "plist",
  "py",
  "rbxl",
  "rbxlx",
  "rbxm",
  "rbxmx",
  "rs",
  "runsettings",
  "service",
  "shortcut",
  "sln",
  "snap",
  "sql",
  "sqlite",
  "sqlite3",
  "tar",
  "tar.gz",
  "tar.xz",
  "tgz",
  "ts",
  "tsv",
  "tsx",
  "txt",
  "typ",
  "tzr.bz2",
  "vsix",
  "wasm",
  "xml",
  "yaml",
  "yml",
  "zip",
];
const FILEPATHS_PROBE_EQUALS = [
  "Cargo.lock",
  "Cargo.toml",
  "Dockerfile",
  "Gemfile",
  "Justfile",
  "LICENSE",
  "Makefile",
  "Procfile",
  "README",
  "Rakefile",
  "Vagrantfile",
  "deny.toml",
  "exact",
  "global.json",
  "nuget.config",
  "project.json",
  "rustfmt.toml",
  ".env",
  ".env.development",
  ".env.development.local",
  ".env.example",
  ".env.local",
  ".env.production",
  ".env.production.local",
  ".env.staging",
  ".env.test",
  ".env.test.local",
  ".env.vault",
];
const FILEPATHS_PROBE_NOISE = ["nofilter.zzzzz", "drop.notanext", ".hidden"];
const FILEPATHS_PROBE_FOLDER = "folder/";
const FILEPATHS_PROBE_NAMES = [
  ...FILEPATHS_PROBE_EXTENSIONS.map((ext) => `keep.${ext}`),
  ...FILEPATHS_PROBE_EQUALS,
  ...FILEPATHS_PROBE_NOISE,
  FILEPATHS_PROBE_FOLDER,
];

function skipWs(source, index) {
  while (index < source.length && /\s/.test(source[index])) index += 1;
  return index;
}

function matchingPair(source, start, open, close) {
  if (source[start] !== open) return -1;
  let depth = 0;
  let quote = null;
  let escape = false;
  for (let index = start; index < source.length; index += 1) {
    const character = source[index];
    if (quote) {
      if (escape) {
        escape = false;
        continue;
      }
      if (character === "\\") {
        escape = true;
        continue;
      }
      if (character === quote) quote = null;
      continue;
    }
    if (character === '"' || character === "'") {
      quote = character;
      continue;
    }
    if (character === open) depth += 1;
    else if (character === close) {
      depth -= 1;
      if (depth === 0) return index;
    }
  }
  return -1;
}

function parseString(source, start) {
  const quote = source[start];
  if (quote !== '"' && quote !== "'") return null;
  let escape = false;
  let value = "";
  for (let index = start + 1; index < source.length; index += 1) {
    const character = source[index];
    if (escape) {
      value += character;
      escape = false;
      continue;
    }
    if (character === "\\") {
      escape = true;
      continue;
    }
    if (character === quote) {
      return { value, end: index + 1 };
    }
    value += character;
  }
  return null;
}

function parseRegex(source, start) {
  if (source[start] !== "/") return null;
  let inClass = false;
  let escape = false;
  for (let index = start + 1; index < source.length; index += 1) {
    const character = source[index];
    if (escape) {
      escape = false;
      continue;
    }
    if (character === "\\") {
      escape = true;
      continue;
    }
    if (character === "[" && !inClass) {
      inClass = true;
      continue;
    }
    if (character === "]" && inClass) {
      inClass = false;
      continue;
    }
    if (character === "/" && !inClass) {
      const pattern = source.slice(start + 1, index);
      let end = index + 1;
      let flags = "";
      while (end < source.length && /[gimsuy]/.test(source[end])) {
        flags += source[end];
        end += 1;
      }
      return { value: { source: pattern, flags }, end };
    }
  }
  return null;
}

function parseArray(source, start) {
  const close = matchingPair(source, start, "[", "]");
  if (close < 0) return null;
  const items = [];
  let index = skipWs(source, start + 1);
  while (index < close) {
    if (source[index] === ",") {
      index = skipWs(source, index + 1);
      continue;
    }
    const string = parseString(source, index);
    if (string) {
      items.push(string.value);
      index = skipWs(source, string.end);
      continue;
    }
    break;
  }
  return { value: items, end: close + 1 };
}

function parseNumber(source, start) {
  const match = source.slice(start).match(/^-?\d+(?:\.\d+)?/);
  if (!match) return null;
  return { value: Number(match[0]), end: start + match[0].length };
}

function parseIdent(source, start) {
  const match = source.slice(start).match(/^[A-Za-z_$][\w$]*/);
  if (!match) return null;
  return { value: match[0], end: start + match[0].length };
}

function parseObjectFields(source, start) {
  const close = matchingPair(source, start, "{", "}");
  if (close < 0) return null;
  const fields = {};
  let index = skipWs(source, start + 1);
  while (index < close) {
    if (source[index] === ",") {
      index = skipWs(source, index + 1);
      continue;
    }
    let key;
    const asString = parseString(source, index);
    if (asString) {
      key = asString.value;
      index = asString.end;
    } else {
      const ident = parseIdent(source, index);
      if (!ident) break;
      key = ident.value;
      index = ident.end;
    }
    index = skipWs(source, index);
    if (source[index] !== ":") break;
    index = skipWs(source, index + 1);
    const parsed = parseValue(source, index);
    if (!parsed) break;
    fields[key] = parsed.value;
    index = skipWs(source, parsed.end);
  }
  return { value: fields, end: close + 1 };
}

function parseValue(source, start) {
  const index = skipWs(source, start);
  if (source.startsWith("!0", index)) {
    return { value: true, end: index + 2 };
  }
  if (source.startsWith("!1", index)) {
    return { value: false, end: index + 2 };
  }
  if (source.startsWith("true", index)) {
    return { value: true, end: index + 4 };
  }
  if (source.startsWith("false", index)) {
    return { value: false, end: index + 5 };
  }
  if (source[index] === '"' || source[index] === "'") {
    return parseString(source, index);
  }
  if (source[index] === "/") {
    return parseRegex(source, index);
  }
  if (source[index] === "[") {
    return parseArray(source, index);
  }
  if (source[index] === "{") {
    return parseObjectFields(source, index);
  }
  return parseNumber(source, index);
}

function asStringList(value) {
  if (typeof value === "string" && value) return [value];
  if (Array.isArray(value)) {
    return value.filter((item) => typeof item === "string" && item);
  }
  return [];
}

export function literalFromObjectFields(fields) {
  if (!fields || typeof fields !== "object") return {};
  const literal = {};
  const extensions = asStringList(fields.extensions);
  if (extensions.length) literal.extensions = extensions;
  const equals = asStringList(fields.equals);
  if (equals.length) literal.equals = equals;
  if (
    fields.matches &&
    typeof fields.matches === "object" &&
    typeof fields.matches.source === "string"
  ) {
    literal.matches = fields.matches.source;
    if (fields.matches.flags) literal.matchesFlags = fields.matches.flags;
  }
  if (typeof fields.showFolders === "string" && fields.showFolders) {
    literal.showFolders = fields.showFolders;
  }
  if (typeof fields.filterFolders === "boolean") {
    literal.filterFolders = fields.filterFolders;
  }
  if (typeof fields.rootDirectory === "string" && fields.rootDirectory) {
    literal.rootDirectory = fields.rootDirectory;
  }
  const filePriority = fields.editFileSuggestions?.priority;
  if (typeof filePriority === "number" && Number.isFinite(filePriority)) {
    literal.filePriority = Math.trunc(filePriority);
  }
  const folderPriority = fields.editFolderSuggestions?.priority;
  if (typeof folderPriority === "number" && Number.isFinite(folderPriority)) {
    literal.folderPriority = Math.trunc(folderPriority);
  }
  return literal;
}

function lastNameBefore(source, index) {
  const window = source.slice(Math.max(0, index - 400), index);
  const matcher = /name\s*:\s*(?:"([^"]+)"|'([^']+)'|\[\s*["']([^"']+)["'])/g;
  let last = "";
  let match;
  while ((match = matcher.exec(window))) {
    last = match[1] || match[2] || match[3] || "";
  }
  return last;
}

const HELPER_CALL_FORMS = [
  { needle: "filepaths)(", kind: "filepaths" },
  { needle: "filepaths(", kind: "filepaths" },
  { needle: "folders)(", kind: "folders" },
  { needle: "folders(", kind: "folders" },
];

function nextHelperCall(source, from) {
  let best = null;
  for (const form of HELPER_CALL_FORMS) {
    const index = source.indexOf(form.needle, from);
    if (index < 0) continue;
    if (best && index >= best.index) continue;
    best = {
      index,
      kind: form.kind,
      paren: index + form.needle.length - 1,
    };
  }
  return best;
}

function parseHelperCall(source, parenIndex, kind) {
  if (source[parenIndex] !== "(") return null;
  const after = skipWs(source, parenIndex + 1);
  if (source[after] === ")") {
    return {
      literal: kind === "folders" ? { showFolders: "only" } : {},
      end: after + 1,
    };
  }
  if (source[after] !== "{") return null;
  const parsed = parseObjectFields(source, after);
  if (!parsed) return null;
  const literal = literalFromObjectFields(parsed.value);
  if (kind === "folders" && literal.showFolders !== "never") {
    literal.showFolders = "only";
  }
  return { literal, end: parsed.end };
}

export function parseFilepathsCalls(source) {
  if (typeof source !== "string" || !source) return [];
  const calls = [];
  let from = 0;
  while (from < source.length) {
    const found = nextHelperCall(source, from);
    if (!found) break;
    const parsed = parseHelperCall(source, found.paren, found.kind);
    if (!parsed) {
      from = found.index + 1;
      continue;
    }
    calls.push({
      start: found.paren,
      precedingName: lastNameBefore(source, found.paren),
      literal: parsed.literal,
      consumed: false,
    });
    from = parsed.end;
  }
  return calls;
}

export function createFilepathsBinder(source) {
  const calls = parseFilepathsCalls(source);
  return {
    take(hints = []) {
      const unused = calls.filter((call) => !call.consumed);
      const pool = unused.length ? unused : calls;
      if (!pool.length) return null;
      const normalized = hints
        .filter((hint) => typeof hint === "string" && hint)
        .map((hint) => hint.toLowerCase());
      if (normalized.length) {
        const ranked = pool
          .map((call) => ({
            call,
            rank: normalized.indexOf(String(call.precedingName).toLowerCase()),
          }))
          .filter((item) => item.rank >= 0)
          .sort((left, right) => left.rank - right.rank);
        if (ranked.length) {
          return ranked[0].call.literal;
        }
        // A single leftover is reused: specs often assign
        // `const files = filepaths({ matches })` and hang it on several args.
        const unnamed = unused.filter((call) => !call.precedingName);
        if (unnamed.length === 1) return unnamed[0].literal;
        if (unused.length === 1) return unused[0].literal;
        return null;
      }
      if (!unused.length) return null;
      unused[0].consumed = true;
      return unused[0].literal;
    },
  };
}

function filepathsProbeExplainedByExtensions(name, extensions) {
  if (!extensions.length) return false;
  const parts = name.split(".");
  if (parts.length < 2) return false;
  let suffix = parts[parts.length - 1];
  for (let i = parts.length - 1; i >= 1; i -= 1) {
    if (extensions.includes(suffix)) return true;
    if (i > 1) suffix = `${parts[i - 1]}.${suffix}`;
  }
  return false;
}

export async function probeFilepathsHelper(gen) {
  let execCwd;
  const mockExec = async (input) => {
    if (input && typeof input.cwd === "string") execCwd = input.cwd;
    return { stdout: FILEPATHS_PROBE_NAMES.join("\n"), status: 0 };
  };
  let rows = [];
  try {
    const result = gen.custom(["cmd"], mockExec, {
      searchTerm: "",
      currentWorkingDirectory: "/probe",
      environmentVariables: { HOME: "/home" },
      isDangerous: false,
    });
    rows = Array.isArray(result) ? result : await result;
    if (!Array.isArray(rows)) rows = [];
  } catch {
    rows = [];
  }
  const names = new Set(
    rows
      .map((row) => (typeof row?.name === "string" ? row.name : ""))
      .filter(Boolean),
  );
  const hasFolder = names.has(FILEPATHS_PROBE_FOLDER);
  const hasDotDot = names.has("../");
  const keptFiles = [...names].filter((name) => !name.endsWith("/"));
  const keptExts = FILEPATHS_PROBE_EXTENSIONS.filter((ext) =>
    names.has(`keep.${ext}`),
  );
  const unfiltered =
    keptExts.length === FILEPATHS_PROBE_EXTENSIONS.length &&
    FILEPATHS_PROBE_EQUALS.every((name) => names.has(name)) &&
    FILEPATHS_PROBE_NOISE.every((name) => names.has(name));
  const extensions = keptExts.filter((ext) => {
    const parts = ext.split(".");
    for (let i = 1; i < parts.length; i += 1) {
      if (keptExts.includes(parts.slice(i).join("."))) return false;
    }
    return true;
  });
  const equals = FILEPATHS_PROBE_EQUALS.filter(
    (name) =>
      names.has(name) && !filepathsProbeExplainedByExtensions(name, extensions),
  );
  const filePriority = rows.find(
    (row) => row && row.type === "file" && typeof row.priority === "number",
  )?.priority;
  const folderPriority = rows.find(
    (row) =>
      row &&
      row.type === "folder" &&
      row.name !== "../" &&
      typeof row.priority === "number",
  )?.priority;
  return {
    hasFiles: keptFiles.length > 0,
    hasFolder,
    hasDotDot,
    unfiltered,
    extensions,
    equals,
    filePriority:
      typeof filePriority === "number" && Number.isFinite(filePriority)
        ? Math.trunc(filePriority)
        : undefined,
    folderPriority:
      typeof folderPriority === "number" && Number.isFinite(folderPriority)
        ? Math.trunc(folderPriority)
        : undefined,
    execCwd,
  };
}

function applyProbeInference(out, probed) {
  if (probed.unfiltered || !probed.hasFiles) return;
  if (
    probed.extensions.length &&
    probed.extensions.length < FILEPATHS_PROBE_EXTENSIONS.length
  ) {
    out.extensions = [...probed.extensions].sort((left, right) =>
      left.localeCompare(right),
    );
  }
  if (probed.equals.length) out.equals = probed.equals;
  if (out.extensions && !probed.hasFolder && probed.hasDotDot) {
    out.filterFolders = true;
  }
}

export function mergeFilepathsNative(literal, probed, gen) {
  const out = { getQueryTerm: "/" };
  const foldersOnly =
    literal?.showFolders === "only" ||
    (typeof gen === "function" &&
      !literal &&
      !probed.hasFiles &&
      (probed.hasFolder || probed.hasDotDot));
  if (foldersOnly) {
    out.templates = ["folders"];
  } else {
    out.templates = ["filepaths"];
    if (literal?.showFolders === "never") {
      out.showFolders = "never";
    } else if (
      !literal &&
      probed.hasFiles &&
      !probed.hasFolder &&
      !probed.hasDotDot
    ) {
      out.showFolders = "never";
    }
  }

  if (literal) {
    if (literal.extensions?.length) {
      out.extensions = [...literal.extensions].sort((left, right) =>
        left.localeCompare(right),
      );
    }
    if (literal.equals?.length) out.equals = literal.equals;
    if (literal.matches) out.matches = literal.matches;
    if (literal.matchesFlags) out.matchesFlags = literal.matchesFlags;
    if (literal.filterFolders) out.filterFolders = true;
    if (literal.rootDirectory) out.rootDirectory = literal.rootDirectory;
    if (literal.filePriority != null) out.filePriority = literal.filePriority;
    if (literal.folderPriority != null) {
      out.folderPriority = literal.folderPriority;
    }
  } else if (!foldersOnly) {
    applyProbeInference(out, probed);
  }

  if (out.filePriority == null && probed.filePriority != null) {
    out.filePriority = probed.filePriority;
  }
  if (out.folderPriority == null && probed.folderPriority != null) {
    out.folderPriority = probed.folderPriority;
  }
  if (!out.rootDirectory && typeof probed.execCwd === "string") {
    const normalized = probed.execCwd.endsWith("/")
      ? probed.execCwd.slice(0, -1)
      : probed.execCwd;
    if (normalized && normalized !== "/probe") {
      out.rootDirectory = probed.execCwd.endsWith("/")
        ? probed.execCwd
        : `${probed.execCwd}/`;
    }
  }
  return out;
}

export async function nativeFilepathsFromHelper(gen, literal) {
  const probed = await probeFilepathsHelper(gen);
  return mergeFilepathsNative(literal ?? null, probed, gen);
}
