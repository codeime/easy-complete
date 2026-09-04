(function (value) {
  function isCandidate(gen) {
    if (!gen || (typeof gen !== "object" && typeof gen !== "function")) return false;
    if (typeof gen.custom !== "function") return false;
    return templatesOf(gen.template).length === 0;
  }

  function templatesOf(value) {
    if (value == null) return [];
    var list = Array.isArray(value) ? value : [value];
    return list.filter(function (item) {
      return item === "filepaths" || item === "folders" || item === "history" || item === "help";
    });
  }

  var EXT = [
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
  var EQ = [
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
  var NOISE = ["nofilter.zzzzz", "drop.notanext", ".hidden"];
  var FOLDER = "folder/";
  var PROBE = EXT.map(function (ext) {
    return "keep." + ext;
  })
    .concat(EQ, NOISE, [FOLDER])
    .join("\n");

  function explainedByExt(name, extensions) {
    if (!extensions.length) return false;
    var parts = name.split(".");
    if (parts.length < 2) return false;
    var suffix = parts[parts.length - 1];
    for (var i = parts.length - 1; i >= 1; i -= 1) {
      if (extensions.indexOf(suffix) !== -1) return true;
      if (i > 1) suffix = parts[i - 1] + "." + suffix;
    }
    return false;
  }

  function nativeFromHelper(gen) {
    var execCwd;
    var usedLs = false;
    var mock = function (input) {
      var args = input && input.args;
      var joined = Array.isArray(args) ? args.join(" ") : String(args || "");
      if (joined.indexOf("-1ApL") !== -1) usedLs = true;
      if (input && typeof input.cwd === "string") execCwd = input.cwd;
      return Promise.resolve({ stdout: PROBE, status: 0 });
    };
    return Promise.resolve()
      .then(function () {
        return gen.custom(["cmd"], mock, {
          searchTerm: "",
          currentWorkingDirectory: "/probe",
          environmentVariables: { HOME: "/home" },
          isDangerous: false,
        });
      })
      .then(function (rows) {
        if (!usedLs) return null;
        if (!Array.isArray(rows)) rows = [];
        var names = {};
        for (var i = 0; i < rows.length; i++) {
          if (rows[i] && typeof rows[i].name === "string") names[rows[i].name] = true;
        }
        var hasFolder = !!names[FOLDER];
        var hasDotDot = !!names["../"];
        var keptFiles = Object.keys(names).filter(function (name) {
          return name.charAt(name.length - 1) !== "/";
        });
        var keptExts = EXT.filter(function (ext) {
          return names["keep." + ext];
        });
        var unfiltered =
          keptExts.length === EXT.length &&
          EQ.every(function (name) {
            return names[name];
          }) &&
          NOISE.every(function (name) {
            return names[name];
          });
        var extensions = keptExts.filter(function (ext) {
          var parts = ext.split(".");
          for (var i = 1; i < parts.length; i += 1) {
            if (keptExts.indexOf(parts.slice(i).join(".")) !== -1) return false;
          }
          return true;
        });
        var equals = EQ.filter(function (name) {
          return names[name] && !explainedByExt(name, extensions);
        });
        var filePriority;
        var folderPriority;
        for (var r = 0; r < rows.length; r += 1) {
          var row = rows[r];
          if (!row || typeof row.priority !== "number") continue;
          if (row.type === "file" && filePriority == null) filePriority = row.priority | 0;
          if (row.type === "folder" && row.name !== "../" && folderPriority == null) {
            folderPriority = row.priority | 0;
          }
        }
        var out = { getQueryTerm: "/" };
        if (keptFiles.length === 0 && (hasFolder || hasDotDot) && typeof gen === "function") {
          out.templates = ["folders"];
          return finish(out, execCwd, filePriority, folderPriority);
        }
        out.templates = ["filepaths"];
        if (keptFiles.length && !hasFolder && !hasDotDot) out.showFolders = "never";
        if (!unfiltered && keptFiles.length) {
          if (extensions.length && extensions.length < EXT.length) {
            out.extensions = extensions.slice().sort();
          }
          if (equals.length) out.equals = equals;
          if (out.extensions && !hasFolder && hasDotDot) out.filterFolders = true;
        }
        return finish(out, execCwd, filePriority, folderPriority);
      })
      .catch(function () {
        return null;
      });
  }

  function finish(out, execCwd, filePriority, folderPriority) {
    if (filePriority != null) out.filePriority = filePriority;
    if (folderPriority != null) out.folderPriority = folderPriority;
    if (typeof execCwd === "string") {
      var normalized = execCwd.charAt(execCwd.length - 1) === "/" ? execCwd.slice(0, -1) : execCwd;
      if (normalized && normalized !== "/probe") {
        out.rootDirectory = execCwd.charAt(execCwd.length - 1) === "/" ? execCwd : execCwd + "/";
      }
    }
    return out;
  }

  function walk(node, seen) {
    if (!node || typeof node !== "object") return Promise.resolve();
    for (var s = 0; s < seen.length; s++) {
      if (seen[s] === node) return Promise.resolve();
    }
    seen.push(node);
    var chain = Promise.resolve();
    function rewriteList(key) {
      var list = node[key];
      if (!list) return;
      var wasArray = Array.isArray(list);
      var arr = wasArray ? list : [list];
      arr.forEach(function (item, index) {
        chain = chain.then(function () {
          if (!isCandidate(item)) return walk(item, seen);
          return nativeFromHelper(item).then(function (native) {
            if (native) arr[index] = native;
            else return walk(item, seen);
          });
        });
      });
      chain = chain.then(function () {
        node[key] = wasArray ? arr : arr[0];
      });
    }
    rewriteList("generators");
    rewriteList("generator");
    ["args", "options", "subcommands"].forEach(function (key) {
      var items = node[key];
      if (!items) return;
      var arr = Array.isArray(items) ? items : [items];
      arr.forEach(function (item) {
        chain = chain.then(function () {
          return walk(item, seen);
        });
      });
    });
    return chain;
  }

  return Promise.resolve(value).then(function (root) {
    return walk(root, []).then(function () {
      return root;
    });
  });
})
