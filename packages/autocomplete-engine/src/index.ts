export type EngineSuggestion = {
  name: string;
  description: string;
  kind: string;
};

export type CompleteInput = {
  buffer: string;
  cwd?: string;
  cursor?: number;
};

export type CompleteOutput = {
  suggestions: EngineSuggestion[];
  search_term: string;
};

const namesOf = (value: unknown): string[] => {
  if (value == null) return [];
  if (Array.isArray(value)) return value.map(String);
  return [String(value)];
};

const asRecord = (value: unknown): Record<string, unknown> | null =>
  value != null && typeof value === "object"
    ? (value as Record<string, unknown>)
    : null;

const descriptionOf = (item: Record<string, unknown>): string =>
  item.description == null ? "" : String(item.description);

function collect(
  items: unknown,
  kind: string,
  query: string,
): EngineSuggestion[] {
  const list = Array.isArray(items)
    ? items
    : items && typeof items === "object"
      ? Object.values(items as Record<string, unknown>)
      : [];
  const out: EngineSuggestion[] = [];
  for (const raw of list) {
    const item = asRecord(raw);
    if (!item) continue;
    const name = namesOf(item.name).find((candidate) =>
      candidate.toLowerCase().startsWith(query.toLowerCase()),
    );
    if (!name) continue;
    out.push({ name, description: descriptionOf(item), kind });
  }
  return out;
}

function tokenize(buffer: string): {
  tokens: string[];
  endsWithSpace: boolean;
} {
  const trimmed = buffer.replace(/\s+$/, "");
  const tokens = trimmed.length === 0 ? [] : trimmed.split(/\s+/);
  return { tokens, endsWithSpace: /\s$/.test(buffer) };
}

type FigSpec = {
  subcommands?: unknown;
  options?: unknown;
  args?: unknown;
  generators?: unknown;
  generator?: unknown;
};

type DenoOps = {
  core?: {
    ops?: {
      op_ec_read_dir?: (path: string) => string[];
      op_ec_read_spec?: (name: string) => string;
      op_ec_execute_shell?: (payload: string) => string;
    };
  };
};

const specCache = new Map<string, FigSpec | null>();

function rewriteSpecSource(source: string): string {
  let rewritten = source.replace(
    /export\s*\{([^}]+)\}/,
    (_match, body: string) => {
      const parts = String(body).split(",");
      const def = parts.find((part) => /\bas\s+default\b/.test(part));
      if (!def) return "return {};";
      const name = def.replace(/\bas\s+default\b/, "").trim();
      return `return ${name};`;
    },
  );
  rewritten = rewritten.replace(/export\s+default\s+/, "return ");
  return rewritten;
}

export async function loadSpec(name: string): Promise<FigSpec | null> {
  if (!name) return null;
  if (specCache.has(name)) return specCache.get(name) ?? null;
  const ops = (globalThis as { Deno?: DenoOps }).Deno?.core?.ops;
  try {
    if (ops?.op_ec_read_spec) {
      const spec = new Function(
        rewriteSpecSource(ops.op_ec_read_spec(name)),
      )() as FigSpec;
      specCache.set(name, spec);
      return spec;
    }
    const mod = (await import(`spec://${name}.js`)) as { default?: FigSpec };
    const spec = mod.default ?? (mod as FigSpec);
    specCache.set(name, spec);
    return spec;
  } catch {
    specCache.set(name, null);
    return null;
  }
}

function findSubcommand(spec: FigSpec, name: string): FigSpec | undefined {
  const items = Array.isArray(spec.subcommands)
    ? spec.subcommands
    : spec.subcommands && typeof spec.subcommands === "object"
      ? Object.values(spec.subcommands)
      : [];
  return items
    .map(asRecord)
    .find((item) => item && namesOf(item.name).includes(name)) as
    FigSpec | undefined;
}

function readDir(path: string): string[] {
  const ops = (globalThis as { Deno?: DenoOps }).Deno?.core?.ops;
  return ops?.op_ec_read_dir?.(path) ?? [];
}

function executeShell(command: string, args: string[], cwd: string): string {
  const ops = (globalThis as { Deno?: DenoOps }).Deno?.core?.ops;
  try {
    return (
      ops?.op_ec_execute_shell?.(JSON.stringify({ command, args, cwd })) ?? ""
    );
  } catch {
    return "";
  }
}

function completePath(prefix: string, cwd: string): EngineSuggestion[] {
  try {
    const base = cwd && !cwd.endsWith("/") ? `${cwd}/` : cwd || "/";
    let dir: string;
    let query: string;
    if (!prefix) {
      dir = base;
      query = "";
    } else if (prefix.endsWith("/")) {
      dir = prefix.startsWith("/") ? prefix : base + prefix;
      query = "";
    } else {
      const slash = prefix.lastIndexOf("/");
      if (slash >= 0) {
        const rawDir = prefix.slice(0, slash + 1);
        dir = rawDir.startsWith("/") ? rawDir : base + rawDir;
        query = prefix.slice(slash + 1);
      } else {
        dir = base;
        query = prefix;
      }
    }
    return readDir(dir)
      .filter((name) => name.toLowerCase().startsWith(query.toLowerCase()))
      .slice(0, 50)
      .map((name) => ({
        name,
        description: name.endsWith("/") ? "Folder" : "File",
        kind: name.endsWith("/") ? "folder" : "file",
      }));
  } catch {
    return [];
  }
}

function pushGenerators(list: unknown[], value: unknown) {
  if (!value) return;
  if (Array.isArray(value)) list.push(...value);
  else list.push(value);
}

function generatorsOf(spec: FigSpec): unknown[] {
  const list: unknown[] = [];
  const args = spec.args;
  if (Array.isArray(args)) {
    for (const arg of args) {
      const item = asRecord(arg);
      pushGenerators(list, item?.generators ?? item?.generator);
    }
  } else {
    const item = asRecord(args);
    pushGenerators(list, item?.generators ?? item?.generator);
  }
  pushGenerators(list, spec.generators ?? spec.generator);
  return list;
}

function templatesOf(spec: FigSpec): string[] {
  const args = spec.args;
  const template = Array.isArray(args)
    ? asRecord(args[0])?.template
    : asRecord(args)?.template;
  if (!template) return [];
  return Array.isArray(template) ? template.map(String) : [String(template)];
}

function runGenerator(
  gen: unknown,
  tokens: string[],
  query: string,
  cwd: string,
): EngineSuggestion[] {
  const item = asRecord(gen);
  if (!item) return [];
  try {
    if (item.script) {
      let out = "";
      const script =
        typeof item.script === "function"
          ? (item.script as (tokens: string[]) => unknown)(tokens)
          : item.script;
      if (typeof script === "string") {
        out = executeShell("sh", ["-c", script], cwd);
      } else if (Array.isArray(script) && script.length > 0) {
        out = executeShell(String(script[0]), script.slice(1).map(String), cwd);
      }
      if (typeof item.postProcess === "function") {
        return collect(
          (item.postProcess as (out: string, tokens: string[]) => unknown)(
            out,
            tokens,
          ),
          "arg",
          query,
        );
      }
      return String(out)
        .split("\n")
        .map((line) => line.trim())
        .filter(
          (line) => line && line.toLowerCase().startsWith(query.toLowerCase()),
        )
        .slice(0, 50)
        .map((name) => ({ name, description: "", kind: "arg" }));
    }
    if (typeof item.custom === "function") {
      type ExecuteOpts = {
        command?: string;
        args?: string[];
        cwd?: string;
      };
      type ExecuteResult = { stdout: string; stderr: string; status: number };
      const execute = (opts: ExecuteOpts): ExecuteResult => ({
        stdout: executeShell(
          opts.command ?? "sh",
          opts.args ?? [],
          opts.cwd ?? cwd,
        ),
        stderr: "",
        status: 0,
      });
      const result = (
        item.custom as (
          tokens: string[],
          execute: (opts: ExecuteOpts) => ExecuteResult,
          context: { currentWorkingDirectory: string; searchTerm: string },
        ) => unknown
      )(tokens, execute, { currentWorkingDirectory: cwd, searchTerm: query });
      if (result && typeof (result as { then?: unknown }).then === "function") {
        return [];
      }
      return collect(result, "arg", query);
    }
  } catch {
    return [];
  }
  return [];
}

/**
 * Headless complete() used by the deno_core engine.
 *
 * Walks Fig specs statically (subcommands / options), runs sync generators, and
 * completes file paths via host ops. Async custom generators and
 * `previewComponent` are out of scope.
 */
export async function complete(input: CompleteInput): Promise<CompleteOutput> {
  const buffer = input.buffer ?? "";
  const cwd = input.cwd ?? "/";
  const { tokens, endsWithSpace } = tokenize(buffer);
  if (tokens.length === 0) {
    return { suggestions: [], search_term: "" };
  }

  const command = tokens[0];
  const spec = await loadSpec(command);
  const searchTerm = endsWithSpace ? "" : (tokens.at(-1) ?? "");
  const query = searchTerm === command ? "" : searchTerm;
  const pathCommands = new Set([
    "cd",
    "ls",
    "cat",
    "rm",
    "mv",
    "cp",
    "open",
    "code",
    "mkdir",
  ]);
  const looksLikePath =
    query.startsWith(".") || query.startsWith("/") || query.includes("/");

  if (!spec) {
    const suggestions =
      pathCommands.has(command) || looksLikePath
        ? completePath(endsWithSpace ? "" : query, cwd)
        : [];
    return { suggestions, search_term: query };
  }

  let current: FigSpec = spec;
  let index = 1;
  while (index < tokens.length - (endsWithSpace ? 0 : 1)) {
    const next = findSubcommand(current, tokens[index]);
    if (!next) break;
    current = next;
    index += 1;
  }

  const includeOptions = query === "" || query.startsWith("-");
  const suggestions = [
    ...collect(current.subcommands, "subcommand", query),
    ...(includeOptions ? collect(current.options, "option", query) : []),
  ];

  if (suggestions.length === 0) {
    for (const gen of generatorsOf(current)) {
      suggestions.push(...runGenerator(gen, tokens, query, cwd));
    }
  }

  const templates = templatesOf(current);
  const wantsPath =
    pathCommands.has(command) ||
    looksLikePath ||
    templates.includes("folders") ||
    templates.includes("filepaths");
  if (suggestions.length === 0 && wantsPath) {
    suggestions.push(...completePath(endsWithSpace ? "" : query, cwd));
  }

  return { suggestions, search_term: query };
}
