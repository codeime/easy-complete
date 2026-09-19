# 纯原生迁移 · 执行任务书

这份文件是给执行 agent 的工作单。总体方案、现状数字和历史结论在 `docs/native-migration-plan.md`；这里只写**怎么做、改哪些文件、怎么验收**。任务编号 T1.x–T4.x 按依赖顺序排列，不要跳做。

## 0. 执行前必读

### 0.1 目标与三条不变量（违反任何一条就是做错了）

- 目标：`Fastab.app` 运行时**不执行任何 JavaScript**，且候选、插入、排序、缓存、shell 环境、超时六个维度的用户可见行为与当前 QuickJS 路径一致。构建期可以用 Node。
- 不变量 1：**未适配项不得静默丢失**。编译器不会还原的行为必须要么编译失败，要么出现在已提交清单里并阻断 gate。任何 "catch 后返回空" 的降级都是违规。
- 不变量 2：**双路径比较只在 dev/test**。正式运行路径在 `inventory.json → gate.pathSwitchAllowed == true` 之前不切换。
- 不变量 3：**发布门槛是依赖树 + 包内容 + 回归测试**（T4.2），不是 "代码删了"。

### 0.2 环境

- Node **必须 ≥ 22.13**（`.mise.toml` 钉 22.23.1；`spec-pair.mjs` 用了 `JSON.parse` reviver 的 `context.source`，Node 20 会假失败 99 个测试）。进仓库目录后 `mise` 会自动激活；如果 `node --version` 不是 22，先 `mise install`。
- Rust 1.88（`rust-toolchain.toml`）。`cargo` 在沙箱里会因 `libsqlite3-sys` 写缓存失败，需要在非沙箱环境跑。
- macOS Apple Silicon 才能跑 `scripts/build-app.sh`；其余脚本和测试在 Linux 也必须通过（CI 的 JavaScript job 是 ubuntu）。

### 0.3 每次改动后的验证命令（按顺序，全部必须通过）

```bash
node scripts/sync-bundled-specs.mjs --check                 # 源 bundle 新鲜
node scripts/compile-spec-ir.mjs                            # 重编 IR（bundle/specs-ir 是 gitignored）
node scripts/audit-spec-hooks.mjs > /dev/null               # 源/IR/hook 审计
node scripts/spec-pair.mjs                                  # 源/IR 配对校验
node scripts/capture-typed-trigger-reference.mjs --check    # 类型化基线（改了 scripts/*.mjs 就要 --update）
node scripts/capture-typed-trigger-reference.mjs --get-query-term --check
node scripts/classify-native-hooks.mjs --check              # 清单（改了分类逻辑/allowlist 就要 --update）
node --test --test-concurrency=1 scripts/*.test.mjs         # 脚本测试
cargo clippy --locked --workspace -- -D warnings
cargo fmt --check
cargo test -p ec_engine
cargo test -p fig_desktop
```

注意：`capture-typed-trigger-reference` 的基线里含 `generatorSha256`（`scripts/compile-spec-ir.mjs` 的 hash）和 `harnessSha256`（若干 `scripts/*.mjs` 的 hash），**改了这些脚本必须 `--update` 两份基线并一起提交**。`bundle/specs/.source-manifest.json` 钉了根 `package.json` 的 hash，改 `package.json` 后要重新跑 `node scripts/sync-bundled-specs.mjs` 并提交 manifest。

### 0.4 提交规则

- 一个任务一个 commit，commit message 写清楚任务编号；每个 commit 上 0.3 的命令全部绿。
- 新增的测试夹具放 `crates/ec_engine/testdata/native-hooks/` 下；临时文件不进仓库。
- 不要改产品默认设置（fuzzy 开、history 显示、firstTokenCompletion 关）；不要恢复 WebView；不要在 `fig_util` 里链接 AppKit；不要把 `figterm` 的 `max_scroll_limit` 改成 0。这些在 `CLAUDE.md` 里都有原因。

### 0.5 必读源码（先读完再动手）

| 文件 | 看什么 |
| --- | --- |
| `CLAUDE.md` | 架构、Completion engine、Bundled Specs 两节 |
| `docs/native-migration-plan.md` | 现状数字、gate、已知偏差 |
| `scripts/spec-hook-contract.mjs` | 9 个 hook 字段、`KNOWN_NON_SPEC_FILES`、`KNOWN_UNAPPLIED_VERSION_DIFFS`、`KNOWN_VERSION_SELECTORS` |
| `scripts/compile-spec-ir.mjs` | `convertNode/convertArg/convertGenerator`、`extractHook`、`bindExtractedHooks`、`closurePreservingHookModule`、`writeTypedHookSidecar`（目前只编 `trigger`） |
| `scripts/typed-hook-ir.mjs` | 现有 typed IR v1：`TYPED_HOOK_CONTRACTS`、`TYPED_EXPRESSION_OPERATIONS`、`compileTypedHook`、`TypedHookCompileError` |
| `crates/ec_engine/src/typed_hook.rs` | Rust 求值器（`#[cfg(test)]`，UTF-16 语义，`deny_unknown_fields`），基线解析器 |
| `crates/ec_engine/src/js_host.rs` | 9 个运行时入口：`post_process/custom/script_command/generate_spec/alias/get_query_term/trigger/load_spec/filter_template_suggestions`；`enter_with_context`；缓存 `cached_suggestions/cached_spec/cached_script_output`；`clean_output`；`spec_from_fig_json`；`merge_generated_spec` |
| `crates/ec_engine/src/generate.rs` | 生成器执行：`generate_for_arg`、`shape_script_output`、`run_script`、trigger 判定（`match trigger.on`）、`GeneratorSession`、debounce |
| `crates/ec_engine/src/lookup.rs` | spec 遍历：`resolve_arg_alias`、`apply_js_load_spec`、`apply_generate_spec`、`next_spec_after_arg`、`complete_with_settings` |
| `crates/ec_engine/src/ir.rs` | `ArgSpec`、`GeneratorSpec`、`GeneratorTrigger`、`Registry` |
| `scripts/classify-native-hooks.mjs` | 清单/gate；`outputBaseline` 目前恒为 `not-yet-established` |
| `scripts/capture-hook-reference-batch.mjs` + `reference-hook-worker.mjs` | 现有合成探针：`FIXTURE_MATRIX`、`$referenceExec` mock、VM 子进程硬超时 |
| `packages/autocomplete-parser/src/*.ts` | Fig 语义的 TS 参考实现（`loadSpec.ts`、`tryResolveSpecToSubcommand.ts`、`loadHelpers.ts`） |
| `git show edf0936a:packages/autocomplete-app/src/state/generators.ts` | WebView 生成器语义原文（`insertion.ts`、`history/index.ts` 同理） |
| `node_modules/.pnpm/@fig+autocomplete-helpers@1.0.7/node_modules/@fig/autocomplete-helpers/dist/esm/src/versions.js` | `applySpecDiff`、`getVersionFromVersionedSpec`、`createVersionedSpec` |

### 0.6 hook 字段契约（JS 签名 → Rust 调用点）

| 字段 | JS 签名 | Rust 调用点 | hook / 函数体 |
| --- | --- | --- | --- |
| `postProcess` | `(out: string, tokens: string[]) => Suggestion[]` | `generate.rs` → `host.post_process` | 2551 / 373 |
| `custom` | `async (tokens, executeCommand, context) => Suggestion[]` | `generate.rs` → `host.custom` | 457 / 103 |
| `getQueryTerm` | `(token: string) => string` | `generate.rs` → `host.get_query_term` | 240 / 16 |
| `trigger` | `(newToken, oldToken) => boolean` | `generate.rs` → `host.trigger`（`trigger.on == "function"`） | 199 / 30（72 hook 已类型化） |
| `script` | `(tokens) => string \| string[] \| {command,args}` | `generate.rs` → `host.script_command` | 190 / 31 |
| `generateSpec` | `async (tokens, executeCommand) => Spec` | `lookup.rs` → `host.generate_spec` | 35 / 32 |
| `filterTemplateSuggestions` | `(suggestions) => Suggestion[]` | `generate.rs` → `host.filter_template_suggestions` | 12 / 5 |
| `alias`（`parserDirectives.alias`） | `async (token, executeCommand) => string` | `lookup.rs` → `host.alias` | 6 / 3 |
| `loadSpec` | `async (token, executeCommand) => Spec \| SpecLocation` | `lookup.rs` → `host.load_spec` | 2 / 1 |
| `getVersionCommand`（版本选择器） | `async (executeCommand) => string` | 无（编译器固定取最高版本） | 5 个 index.js |
| `versions` diff（版本文件） | `Record<version, SpecDiff>` | 无（编译器忽略，已 allowlist） | 15 个 diff / 9 个函数 |

`context`（`custom` 的第三个参数）字段：`currentWorkingDirectory`、`currentProcess`、`sshPrefix`、`environmentVariables`、`searchTerm`、`isDangerous`。Rust 侧由 `JsHost::enter_with_context` 从 `CompleteRequest` 喂入。

---

## 阶段 1：清单与输出基线

### T1.1 基线文件格式与解析器

**目标**：定义每个不同函数体一份的输出基线格式，JS 与 Rust 两端都能严格解析。

**改动**

- 新建 `scripts/hook-baseline-contract.mjs`：导出 `BASELINE_VERSION = 1`、`BASELINE_KIND = "native-hook-baseline"`、每字段的参数契约（参数个数、类型、`exec` 位置）、`validateBaseline(value)`（未知字段抛错）。
- 基线路径：`crates/ec_engine/testdata/native-hooks/baseline/<sourceField>/<bodySha256>.json`。
- 文件形状：

```json
{
  "version": 1,
  "kind": "native-hook-baseline",
  "field": "postProcess",
  "bodySha256": "…",
  "representativeHookId": "git#postProcess#3",
  "hookCount": 12,
  "cases": [
    {
      "id": "normal",
      "args": ["<stdout 原文>", ["git", "checkout", ""]],
      "exec": [{ "command": "git", "args": ["branch"], "stdout": "…", "stderr": "", "status": 0 }],
      "context": { "currentWorkingDirectory": "/repo", "currentProcess": "zsh", "sshPrefix": "", "environmentVariables": { "HOME": "/Users/x" }, "searchTerm": "", "isDangerous": false },
      "timeoutMs": 5000,
      "expected": { "kind": "suggestions", "value": [ { "name": "main", "insertValue": "main", "description": "", "icon": "", "priority": 50, "type": "arg" } ] }
    }
  ]
}
```

`expected.kind` ∈ `suggestions | string | bool | argv | spec | error | timeout`。`suggestions` 里的每个对象字段名与 `Fig.Suggestion` 一致，缺省字段不写；`spec` 用 `spec_from_fig_json` 能解析的 Fig JSON。
- Rust：在 `crates/ec_engine/src/typed_hook.rs` 旁新建 `crates/ec_engine/src/hook_baseline.rs`（`#[cfg(test)]`），用 `serde(deny_unknown_fields)` 解析同一格式，并提供 `load_all() -> Vec<Baseline>`。在 `lib.rs` 里 `#[cfg(test)] mod hook_baseline;`。

**验收**

- `node --test scripts/hook-baseline-contract.test.mjs`：合法样例通过；多一个未知字段、少一个必填字段、`args` 个数与字段契约不符都抛错。
- `cargo test -p ec_engine hook_baseline`：同一份样例 Rust 能解析；多一个字段解析失败。

### T1.2 基线采集器

**目标**：对全部 594 种函数体，用**源闭包**（不是生成的模块）采集输出，写成 T1.1 的格式；提供 `--check/--update`。

**改动**

- 新建 `scripts/capture-hook-baseline.mjs`。复用 `reference-hook-worker.mjs` 的 VM 子进程（mock exec 通过 `mockExecRules` 精确匹配 `{command,args}`，硬超时），不要新写执行器。
- 输入来源：每个函数体一个**输入夹具** `crates/ec_engine/testdata/native-hooks/inputs/<sourceField>/<bodySha256>.json`（形状 = 基线去掉 `expected`）。采集器读输入夹具 → 跑源闭包 → 写基线。
- 输入夹具生成器 `scripts/scaffold-hook-inputs.mjs`：对没有输入夹具的函数体，按字段生成默认 3 个 case：
  - `postProcess`：`normal`（见 T1.3 的真实样本；没有则合成）、`empty`（`""`）、`malformed`（`"not json\n{{{"`）。`tokens` 取该函数体任一 hook 所在 spec 的根命令 + 子命令路径（从 IR 的 `path` 反推）+ `""`。
  - `script`：`tokens` 三种：只有根命令、带一个子命令、带 `--flag`。
  - `custom`：`tokens` 同上；`exec` 规则由**运行一次记录**得到：先用 "记录模式" 跑闭包，把它请求的每个 `{command,args}` 记下来，从 T1.3 样本库取输出，取不到就 `status: 127, stderr: "command not found"`；再正式采集。
  - `trigger`：`(("a","ab"), ("src/", "src"), ("", "x"), ("a/b", "a/b/"), ("foo:bar", "foo:"))` 五组。
  - `getQueryTerm`：`("", "a", "src/main.rs", "a:b:c", "--flag=value", "user@host:path")` 六组。
  - `filterTemplateSuggestions`：一组 8 条混合 file/folder 建议。
  - `alias` / `loadSpec` / `generateSpec`：`tokens`/`token` + 记录模式得到的 `exec`。
  - 每个字段额外一条 `timeout` case：`timeoutMs: 50`，`exec` 规则返回 `{ "delayMs": 10000 }`（需要在 `reference-hook-worker.mjs` 的 mock exec 上加 `delayMs` 支持），期望 `expected.kind == "timeout"`。
- `--check`：重新采集并与已提交基线逐字节比较；`--update`：覆盖写入。基线里不能有时间戳、绝对路径、机器信息。
- 采集必须**完全离线、不执行真实命令**（现有 worker 已保证；不要放开）。

**验收**

- `node scripts/capture-hook-baseline.mjs --update` 后，`baseline/` 下正好 594 个文件，`cases.length ≥ 3`，每个字段至少一个 `timeout` case。
- 连续两次 `--check` 通过（确定性）。
- 新增测试 `scripts/capture-hook-baseline.test.mjs`：小夹具的采集、`--check` 对篡改基线报错、mock `delayMs` 触发超时。
- CI（`.github/workflows/ci.yml` JavaScript job）加 `node scripts/capture-hook-baseline.mjs --check`。

### T1.3 真实 CLI 输出样本库

**目标**：`postProcess`/`custom` 的 `normal` case 用真实输出，不用合成字符串。

**改动**

- 新建 `scripts/record-cli-output.mjs`：读 IR 中所有 `script`（`ArgSpec.script` / `GeneratorSpec.script`，以及 T1.2 记录模式收集到的 `custom` 请求），去重后在**开发机**上执行并录制到 `crates/ec_engine/testdata/native-hooks/cli-output/<sha256(JSON argv)>.json`：`{ "argv": [...], "stdout": "...", "stderr": "...", "status": 0 }`。
- 脱敏：录制前用 `scripts/record-cli-output.mjs --redact` 把 `$HOME`、用户名、IP、token 形状（`ghp_`、`sk-`、`AKIA`、40 位 hex）替换为占位符；命令白名单在脚本里显式列出（`git`、`npm`、`pnpm`、`yarn`、`docker`、`kubectl`、`brew`、`cargo`、`gh`、`aws-vault` 等只读子命令），不在白名单的命令不执行、写 `status: 127`。
- 每条样本 ≤ 64 KB（超出截断到前 64 KB 并标 `"truncated": true`）。
- T1.2 的 scaffold 优先取样本库，其次合成。

**验收**

- 至少覆盖 373 个 `postProcess` 函数体中 ≥ 300 个的 `normal` case 用真实样本（其余函数体所依赖的 CLI 未安装时允许合成，并在输入夹具里标 `"synthetic": true`）。
- `git grep -c` 样本库里没有 `/Users/`、`ghp_`、`sk-`。

### T1.4 清单接入基线

**目标**：`inventory.json` 的 `outputBaseline` 从占位符变成真实覆盖数。

**改动**

- `scripts/classify-native-hooks.mjs`：读 `baseline/` 目录，对每个 body group 标 `baselineCovered: true/false`、`baselineCases: n`；`outputBaseline = { status: covered==total ? "established" : "partial", coveredUniqueBodies, totalUniqueBodies }`；覆盖不全时 gate 保留 `output-baseline-not-established`。
- `pathSwitchAllowed` 增加条件 `outputBaseline.status === "established"`。

**验收**

- `node scripts/classify-native-hooks.mjs --update && --check` 通过；`inventory.json` 中 `outputBaseline.coveredUniqueBodies == 594`，blocker 里不再有 `output-baseline-not-established`。
- `scripts/classify-native-hooks.test.mjs` 加：部分覆盖时 blocker 存在、全覆盖时消失。

### T1.5 引擎级 golden（六个维度）

**目标**：把 `crates/ec_engine/testdata/phase1/expected.json` 从 "静态 IR" 扩到六个维度，用 mock exec 锁定 `CompleteResult`。

**改动**

- `crates/ec_engine/src/runtime.rs` 的 `phase1_static_ir_complete_result_golden` 旁新增 `engine_golden_six_dimensions`；夹具目录 `testdata/engine-golden/`，每条 case：`{ name, request: CompleteRequest, exec: [...mock], settings: {...}, result }`。
- 需要在 `crates/ec_engine/src/process.rs`（`process::execute`）加一个 `#[cfg(test)]` 的 mock 注入点（thread-local 规则表），让 golden 不真的跑命令。
- 维度与最少 case 数：
  - 候选：`git checkout `、`npm run `、`docker run `、`kubectl get `、`cd `、`ls -` 各 3 条（含 `name/insert_value/description/icon/priority/kind/should_add_space`）。
  - 插入：`getQueryTerm` 影响 `query_term`（`cd src/m`、`asdf install nodejs:`）、`insertValue` 带 `{cursor}`（`git commit -m "{cursor}"`）、带空格路径引号，≥ 6 条。
  - 排序：同名 history vs spec 行、`priority` 差、`fuzzy` 开关，≥ 6 条。
  - 缓存：同一 `generatorArgId` 两次请求第二次不触发 exec（用 mock 计数）、`cacheByDirectory` 不同 cwd 触发、`ttl` 过期 SWR 返回旧行，≥ 4 条。
  - shell 环境：`custom` 读 `environmentVariables`/`currentProcess` 的 spec（在 inventory 里筛 `dependencies.environment == true` 的函数体，例如 `env`、`kubectl` 的 context 相关），≥ 3 条。
  - 超时：`scriptTimeout` 设置生效（mock `delayMs` > 设置值 → 空结果，`HookDiagnostic` 记录 timeout），≥ 2 条。
- golden 的生成用 `EC_ENGINE_GOLDEN_UPDATE=1 cargo test ... engine_golden` 覆盖写入；默认比较。

**验收**

- `cargo test -p ec_engine engine_golden` 通过，case 数 ≥ 40；连续两次运行结果一致。

**阶段 1 完成定义**：T1.1–T1.5 全部合入；`inventory.json` 的 `outputBaseline.status == "established"`；CI 有 baseline `--check`。

---

## 阶段 2：类型化 IR + 原生适配

原则：能静态表达的编成 typed IR；不能的写具名 Rust 适配器；两者都没有 → 编译失败。阶段 2 结束时 `counts.uniqueBodies["requires-native-adapter"] == 0`。

### T2.1 Typed IR v2：值与字符串运算

**目标**：扩展 `scripts/typed-hook-ir.mjs` 与 `crates/ec_engine/src/typed_hook.rs`，两端同步。

**改动**（两端各一份，所有新 op 都要进 `TYPED_EXPRESSION_OPERATIONS` 和 Rust `TypedExpr` 枚举，Rust 侧 `deny_unknown_fields` 保留）

- 新值类型：`json`（`JSON.parse` 结果的带标签树）、`suggestion`、`suggestion-array`、`string-record`、`null`。`TYPED_VALUE_TYPES` 同步。
- 字符串 op：`string-trim/trim-start/trim-end`、`string-replace`（字面量 needle）、`string-replace-all`、`string-starts-with/ends-with`、`string-substring`、`string-last-index-of`、`string-to-lower/upper`、`string-pad-start/pad-end`、`string-repeat`、`string-concat`（模板字符串编成 concat）、`string-char-at`、`string-at`。
- 数值 op：`add/sub/mul`（安全整数范围检查，溢出 fail closed）、`lt/le/ge`、`not`。
- 一元/逻辑：`not`、`nullish`（`??`）。
- 编译器：`compileTypedHook({ body, sourceField })` 支持新契约（见 T2.3），并加 `MAX_NODES` 到 512、`MAX_DEPTH` 到 24（同步 Rust 常量）。
- 每个新 op 必须有：JS 单测（`typed-hook-ir.test.mjs`）、Rust 单测、以及一条跨语言 golden（把 JS 求值结果与 Rust 求值结果对同一 descriptor 比较——用 T1.2 的基线 case 作为输入）。UTF-16 语义：`length/indexOf/slice/substring/charAt/padStart` 全部按 code unit。

**验收**

- `node scripts/classify-native-hooks.mjs` 中 `trigger` 与 `getQueryTerm` 的 `typed-ir-research-candidate + 已类型化` 覆盖全部 30 + 16 个函数体（不依赖 T2.2 的数组 op 的那部分）。
- 两端测试通过；`cargo test -p ec_engine typed_hook` 通过。

### T2.2 Typed IR v2：数组、对象、控制流、正则、JSON

**改动**

- 数组 op：`array-map/filter/flat-map/slice/join/some/every/find/find-index/includes/index-of/length/concat/reverse/sort`；lambda 用 `{op:"lambda", params:[...], body}`，作用域只允许引用 lambda 参数和外层 `let` 绑定。`sort` 比较器只接受三种形状：`(a,b) => a.localeCompare(b)`、`(a,b) => a.x - b.x`、`(a,b) => a < b ? -1 : 1`。
- 对象：`object`（键限定为 `Fig.Suggestion` 字段：`name/displayName/insertValue/description/icon/priority/hidden/isDangerous/type/args/replaceValue/deprecated`），`spread`（只允许展开 `suggestion` 类型），`get`（JSON 路径访问，缺失→`null`）。
- 控制流：`let`（块内单次赋值）、`if` 语句、`return` 提前返回、`try`（`catch` 分支只能返回常量或参数）、`for-of`（无 `break/continue` 以外副作用）。
- 正则：只接受**字面量**；编译期用 `fancy-regex`（Rust 侧）能接受的子集校验，翻译规则写在 `scripts/typed-regex.mjs`（命名组、`\d\w\s`、量词、锚点、非贪婪、前瞻；拒绝 `v` flag、后顾中的可变长度、Unicode 属性转义以外的 `\p`）。op：`regex-test/regex-match/regex-match-all/regex-replace/string-split-regex`。
- JSON：`json-parse`（失败 → `null`，供 `try/catch` 语义）、`json-get`、`json-array-items`、`json-as-string/number/bool`。
- **模块级 helper 内联**：在 `compile-spec-ir.mjs` 中，对 hook 的自由变量（`freeVariableCandidates` 已能算出）用 acorn + eslint-scope 找到模块顶层声明：
  - `const X = <literal|array|object literal>` → 折叠为常量；
  - `const X = (args) => …` / `function X(args) {…}` 且 X 不引用宿主对象 → β-归约内联到调用点（每个 hook 内联总节点上限 2048，递归拒绝）；
  - 引用 `fig.*`、`window`、`process`、`require`、`console`、`Intl`、`Date.now`、`Math.random` 的 helper → fail closed，进 T2.4。
- 编译器新增诊断输出：`node scripts/compile-spec-ir.mjs --typed-report` 打印按字段、按 `TypedHookCompileError.code` 聚合的失败原因 Top 20，指导下一步加哪个 op。

**验收**

- `postProcess` 373 个函数体中 ≥ 330 个编译成 typed IR；`script` 31/31；`filterTemplateSuggestions` 5/5；`trigger` 30/30；`getQueryTerm` 16/16。剩余的进 T2.4。
- 每个类型化的函数体，用 T1.2 基线的全部 case 在 Rust 求值器上跑，结果逐字节等于 `expected`（写成 `cargo test -p ec_engine typed_hook_baseline_parity`）。

### T2.3 生产 sidecar 扩到全部无副作用字段

**改动**

- `scripts/spec-hook-contract.mjs` / `typed-hook-ir.mjs` 的 `TYPED_HOOK_CONTRACTS` 加 `postProcess`（`params: ["string","string-array"]`, `resultType: "suggestion-array"`）、`script`（`["string-array"] → "string-array"`）、`getQueryTerm`（转正）、`filterTemplateSuggestions`（`["suggestion-array"] → "suggestion-array"`）。
- `compile-spec-ir.mjs` 的 `writeTypedHookSidecar` 遍历这些字段（现在只 `trigger`）；`audit-spec-hooks.mjs` 的 typed 校验同步（`orphanTypedHooks/typedHookMismatches`）。
- `capture-typed-trigger-reference.mjs` 泛化为 `capture-typed-reference.mjs --field <f>`，每个字段一份 `crates/ec_engine/testdata/typed-hooks/<field>-reference.json`（沿用现有 provenance 字段）。CI 全部 `--check`。
- Rust `typed_hook.rs`：`evaluate_typed_post_process/script/get_query_term/filter_template_suggestions`，输入输出类型与 `js_host.rs` 对应方法一致（`Vec<Suggestion>`、`ScriptCommand`、`String`）。

**验收**

- `typed-hooks.json` 覆盖的 hook 数 = 各字段类型化函数体对应的 hook 总数；`audit-spec-hooks.mjs` 零错误。
- `inventory.json` 里 `typed-ir-research-candidate` 归零（研究状态删除，只剩 `typed-ir`/`native-adapter`/`requires-native-adapter`）。

### T2.4 具名原生适配器

**目标**：编不出 typed IR 的函数体，用 Rust 实现，按 `bodySha256` 绑定。

**改动**

- 新建 `crates/ec_engine/src/native_adapters/mod.rs` + 每字段一个子模块；注册表 `static ADAPTERS: &[(&str /*bodySha256*/, &str /*field*/, AdapterFn)]`。
- 每个适配器文件头注释写：函数体 sha、代表 hook id、JS 原文（从 `bundle/specs-ir/hooks/<id>.js` 拷贝）、为什么不能 typed。
- 编译器：`compile-spec-ir.mjs` 读 `crates/ec_engine/testdata/native-hooks/adapters.json`（新增 `crates/ec_engine/examples/dump-adapters.rs`，`cargo run -p ec_engine --example dump-adapters` 生成并提交，列出全部已注册 sha；再加一个 Rust 测试断言该文件与注册表一致），typed 失败且不在 adapters 里的函数体 → **编译失败**（把今天的 `requires-native-adapter` 桶变成硬错误；可用 `EC_ALLOW_UNADAPTED=1` 临时放行以便分阶段合并，CI 不设置）。
- 每个适配器必须在 `cargo test -p ec_engine native_adapters_baseline_parity` 里对 T1.2 基线全部 case 通过。

**验收**

- `inventory.json`：`requires-native-adapter == 0`；`adapters.json` 里的 sha 数 = `native-adapter` 函数体数（预计 30–60）。
- 不允许出现 "适配器返回空列表" 的兜底实现——空结果只能在基线 `expected` 也是空时出现。

### T2.5 带副作用的字段：`custom` / `alias` / `loadSpec` / `generateSpec`

**改动**

- typed IR 效应节点：`exec { command, args, cwd?, env?, timeout? } → { stdout, stderr, status }`（`args`、`command` 必须是 typed 表达式，不允许字符串拼接出的 shell 命令——`sh -c` 只在源码就是 `sh -c` 时保留）；`await` 顺序编成 `seq`；`Promise.all([...])` 编成 `par`。
- `context` 访问 op：`ctx-cwd/ctx-process/ctx-ssh-prefix/ctx-env(name)/ctx-search-term/ctx-is-dangerous`。
- `generateSpec`/`loadSpec` 返回值：`spec-object` op（键限定为 Fig Spec 已知字段），Rust 侧转成 `Spec` 后走现有 `merge_generated_spec` / `next_spec_after_arg`。
- Rust：`typed_hook.rs` 增加 `evaluate_typed_custom(descriptor, tokens, ctx, exec: &dyn Fn(ExecRequest)->ExecResult, deadline)`，`exec` 由 `process::execute` 实现，并沿用 `js_host` 现有的 deadline 钳制（每次 exec 不超过剩余预算）。
- 编不出的 → T2.4 适配器（`custom` 预计最多）。

**验收**

- 四个字段全部函数体 ∈ {typed, adapter}；基线 parity 测试通过（`exec` 用基线里的 mock 规则）。

### T2.6 版本化 spec 适配

**改动**

- `scripts/spec-versions.mjs`：从 `@fig/autocomplete-helpers` 移植 `applySpecDiff`（保持算法逐行一致，加单测对照原库输出）。
- `compile-spec-ir.mjs`：对导出 `versions` 的版本文件，按 semver 升序逐级应用 diff，为**每个可选版本键**各输出一份 IR：`<dir>/<fileVersion>.json`（基础）和 `<dir>/<fileVersion>+<diffVersion>.json`（应用到该键）；diff 里的函数按 T2.1–T2.5 处理（注意 `bindExtractedHooks` 以函数身份绑定，diff 合并用 `Object.assign` 保留身份；`closurePreservingHookModule` 需要在模块内也做一次同样的合并——把移植的 `applySpecDiff` 作为模块前缀注入，路径以合并后对象为准）。
- `index.json` 新增 `versioned: { "<command>": { "command": [...argv], "regex": "…", "fallback": "8.0.0", "files": { "8.0.0": "heroku/8.0.0.json", "8.6.0": "heroku/8.6.0.json", … } } }`；5 个 `getVersionCommand` 编成 typed IR（`exec` + 正则）。
- Rust `ir.rs`/`lookup.rs`：`Registry::get_arc` / `lookup.rs::root_spec_for_command` 遇到 `versioned` 条目时，按会话缓存运行版本命令（`process::execute_full`，超时用 `autocomplete.scriptTimeout`），选择 ≤ 版本的最高文件；命令失败 → 最高文件（与 WebView 一致）。
- 删除 `KNOWN_UNAPPLIED_VERSION_DIFFS` / `KNOWN_VERSION_SELECTORS`，`classify-native-hooks.mjs` 的 `versionedSpecs` 改为报告 "adapted" 并移除 blocker。

**验收**

- `ec engine complete --buffer "fig "` 的候选包含 2.16.0 diff 加入的子命令；`heroku` 在 mock `heroku --version` = `8.3.0` 时走 8.0.0 文件并应用 8.11.1 diff（WebView 的原始行为）。
- `inventory.json` 无 `versioned-spec-behaviour-unadapted`。

**阶段 2 完成定义**：`inventory.json` 中 `uniqueBodies` 只剩 `typed-ir` 与 `native-adapter` 两类，其余为 0；全部基线 parity 测试通过；编译器对新增未适配 hook 硬失败。

---

## 阶段 3：双路径比较（仅 dev/test）

### T3.1 运行时后端抽象

**改动**

- `crates/ec_engine/Cargo.toml` 加 feature `js-compat`（默认开），`rquickjs` 依赖改为 `optional = true` 挂在该 feature 下。
- `lib.rs`：`typed_hook` 与 `native_adapters` 去掉 `#[cfg(test)]`。
- 新建 `crates/ec_engine/src/hook_backend.rs`：`enum HookBackend { Native, Js }`，`fn current() -> HookBackend`（默认值由 `EC_HOOK_BACKEND` 环境变量或设置 `autocomplete.hookBackend` 决定，**默认仍为 `Js`**，直到 T3.4）。
- 把 0.6 表里的 9 个调用点改为经 `hook_backend::dispatch_*`：Native → typed 求值器/适配器（按 hook id 查 `typed-hooks.json` → 无则查 adapters）；Js → 现有 `JsHost`。Native 路径找不到实现时**返回错误并记 `HookDiagnostic`**，不回落到 Js（回落会掩盖阶段 2 的漏网）。

**验收**

- `cargo test -p ec_engine`、`cargo test -p ec_engine --no-default-features` 都通过（后者不链接 rquickjs，`js_host` 整体 `#[cfg(feature = "js-compat")]`）。
- `cargo tree -p ec_engine --no-default-features -e normal | grep -c rquickjs` 为 0。

### T3.2 双路径测试

**改动**

- `crates/ec_engine/src/dual_path.rs`（`#[cfg(all(test, feature = "js-compat"))]`）：对 T1.2 基线全部 case 和 T1.5 引擎 golden，分别用 Native/Js 跑，`serde_json::to_value` 后逐字节比较。允许的归一化必须逐条写在 `DUAL_PATH_NORMALISATIONS` 常量里并注释原因（例如 JS `-0` vs Rust `0`）。
- 输出差异报告到 `target/dual-path-report.json`（测试失败时）。

**验收**

- `cargo test -p ec_engine dual_path` 零差异。

### T3.3 真实场景回放

**改动**

- `crates/ec_cli/src/cli/engine.rs`：`ec engine complete` 加 `--compare`（两路都跑，打印 JSON diff，非零退出表示有差异）。
- `scripts/dual-path-session.sh`：读 `tests/dual-path/sessions/*.jsonl`（每行 `{buffer, cwd}`），在 git/npm/docker/kubectl/cargo 五类真实仓库目录下逐条调用 `ec engine complete --compare`。录制 5 个 session，每个 ≥ 100 条 buffer（含逐字符输入序列）。T4.1 删掉 `--compare` 之后，脚本与数据改名为 `scripts/replay-sessions.sh` / `tests/session-replay/`，只做冒烟回放。
- `fig_desktop`：新写一个测试驱动器（CLAUDE.md 的 Native UI 一节描述了帧格式：在 `remote.sock` 握手，向 `desktop.sock` 发 `EditBufferHook` + caret 帧，caret 帧编码见 `crates/fig_input_method/src/wire.rs`），回放 T3.3 的 session，`EC_HOOK_BACKEND=native` 下 overlay 测试全部通过。

**验收**

- 5 个 session 零差异；`cargo test -p fig_desktop` 在两种后端下通过；人工在 Terminal.app / iTerm2 / Ghostty / VS Code 各输入 20 个常用命令无回归（记录在 PR 描述里）。

### T3.4 切换默认后端

**前置**：`node scripts/classify-native-hooks.mjs --check` 输出的 `gate.pathSwitchAllowed == true`。把该条件写进 CI（`inventory.json` 中该字段为 `false` 时 T3.4 的 commit 不得合并——在 CI 加一步 `node -e "…assert(gate.pathSwitchAllowed)"`，此前用 `EC_GATE_PATH_SWITCH_EXPECTED=false` 门控）。

**改动**：`HookBackend::current()` 默认改为 `Native`；`js-compat` 保留一个版本；CHANGELOG 记录。

---

## 阶段 4：删除运行时 JS

### T4.1 删除 ✅

- `rquickjs` 依赖、`js_host.rs`、`hook_backend.rs` 的 Js 分支、`snapshot.rs` 里模块校验相关代码、`js-compat` feature。
- `compile-spec-ir.mjs` 停止输出 `hooks/`、`source-modules/`、`hook-modules.json`；`audit-spec-hooks.mjs`/`spec-pair.mjs`/`build-app.sh` 中相应检查改为 "出现即失败"。
- `capture-hook-reference*.mjs`、`reference-hook-worker.mjs` 保留为**构建期**基线采集工具（它们跑的是 Node，不进 `.app`）。

### T4.2 发布门槛（写进 `ci.yml` 与 `release.yml`） ✅

```bash
test "$(cargo tree -p fig_desktop -e normal | grep -c rquickjs)" = 0
test -z "$(find 'build/Fastab.app/Contents/Resources' -name '*.js' -o -name '*.mjs')"
test "$(du -sm 'build/Fastab.app/Contents/Resources/specs-ir' | cut -f1)" -le 35
cargo test --workspace --locked        # 基线 parity + 引擎 golden + fig_desktop
```

### T4.3 文档 ✅

- `CLAUDE.md`：重写 Completion engine 的 hook 表（typed IR / adapter 两类）与 Bundled Specs 段落；删除 QuickJS 相关段落；把 "JsHost.sources 缓存" 等描述删掉。
- `docs/native-migration-plan.md` 标记完成；CHANGELOG（中英）记 "桌面零运行时 JS"。

---

## 5. 进度报告格式

每完成一个任务，在 PR/commit 描述里贴：

```
T<编号> <标题>
- 改动文件：…
- 验收命令与结果：…
- inventory.json 变化：requires-native-adapter 580 → 5xx, typed-ir 9 → 1xx, outputBaseline 0/594 → 594/594 …
- 未完成/放到后续的：…
```

数字一律以 `node scripts/classify-native-hooks.mjs` 的输出为准，不要手算。
