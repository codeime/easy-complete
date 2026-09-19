# 纯原生补全迁移计划（桌面零运行时 JS）

**状态（T4.3）：完成。** `Fastab.app` 运行时不执行 JavaScript。3701 个抽取 hook = 3136 typed IR + 565 named adapters；`hookFilesOnDisk = 0`；`gate.pathSwitchAllowed = true`；`rquickjs` / `js_host` / `hooks/` / `source-modules/` 已删除。构建期仍用 Node。清单以 `crates/ec_engine/testdata/native-hooks/inventory.json` 为准。

目标：`Fastab.app` 运行时不执行任何 JavaScript（删除 `rquickjs`、`js_host`、`hooks/`、`source-modules/`），同时用户可见行为（候选、插入、排序、缓存、shell 环境、超时）与原先的 QuickJS 路径 / WebView v2.2.2 等价。构建期可以用 Node；`.app` 里不能有 JS。

三条不变量贯穿全部阶段：

1. **未适配项不得静默丢失。** 任何编译器不会还原的动态行为都必须出现在已提交的清单里并阻断切换 gate；编译器对未列出的项 fail closed。
2. **仅在 dev/test 双路径比较。** 正式运行路径在全部 bundled hook 达标前不切换。
3. **发布门槛是依赖树 + 包内容 + 回归测试**，不是“代码删掉了”。

逐任务的执行说明（改哪些文件、验收命令）在 `docs/native-migration-tasks.md`。进度、数字与 gate 的单一事实来源是 `crates/ec_engine/testdata/native-hooks/inventory.json`（`node scripts/classify-native-hooks.mjs --check|--update`，CI 校验）。下面所有数字均来自 `@chen86860/autocomplete-specs@3.1.0` 的这份清单。

## 0. 现状快照（v3.0.0-beta.14）

> 这一节是 T4 切换前的基线，不是当前 Fastab 运行时。当前状态见文首：typed IR 已上线，`hookFilesOnDisk = 0`，`pathSwitchAllowed = true`。

源 spec 中共 **4268** 个 hook 函数：

- **576** 个已在编译期被 `filepaths()` helper 的原生重写吃掉（`custom`/`trigger`/`getQueryTerm` 各 192，来自 73 个 spec）。这部分已经是纯原生。
- **3692** 个被抽取成 JS hook（**594** 种不同函数体），运行时全部仍由 QuickJS 执行。

| 字段 | hook 数 | 不同函数体 | 调用 `exec` 的函数体 | 读 shell 环境 | 已类型化 |
| --- | ---: | ---: | ---: | ---: | --- |
| `postProcess` | 2551 | 373 | 0 | 0 | 0（4 个研究候选） |
| `custom` | 457 | 103 | 70 | 22 | 0 |
| `getQueryTerm` | 240 | 16 | 0 | 0 | 0（2 个研究基线：`asdf`） |
| `trigger` | 199 | 30 | 0 | 0 | **72 hook / 9 函数体**（生产 sidecar `typed-hooks.json`，Rust 求值器 `#[cfg(test)]`） |
| `script` | 190 | 31 | 0 | 0 | 0（2 个研究候选） |
| `generateSpec` | 35 | 32 | 31 | 0 | 0 |
| `filterTemplateSuggestions` | 12 | 5 | 0 | 0 | 0 |
| `alias` | 6 | 3 | 3 | 0 | 0 |
| `loadSpec` | 2 | 1 | 0 | 0 | 0 |

其他未适配的动态行为（不是 hook，但 WebView 在加载 spec 时执行过）：

- **5 个版本选择器**（`createVersionedSpec` 的 `index.js`：`fig`、`heroku`、`shopify`、`infracost`、`@usermn/sdc`）。WebView 跑 `<cli> --version` 选版本文件；编译器固定取最高版本文件（等价于 WebView 在 CLI 不存在或版本高于所有文件时的选择）。
- **15 个版本 diff**（`versions` 导出，4 个文件，含 9 个函数）。WebView 用 `getVersionFromVersionedSpec` 把 ≤ 检测版本的 diff 合并进 spec；编译器目前一个都不合并。已在 `KNOWN_UNAPPLIED_VERSION_DIFFS` 中逐项列出，编译器对未列出 / 已过期的项 fail closed。

gate 当前阻断项（`inventory.json` → `gate.blockers`）：`requires-native-adapter`（580 函数体）、`typed-ir-research-candidate`（14 函数体）、`output-baseline-not-established`（0/594）、`versioned-spec-behaviour-unadapted`。`pathSwitchAllowed: false`。

一个对阶段 2 很重要的观察：spec 是 esbuild 压缩产物，hook 里的自由变量几乎全是模块级压缩标识符（`e`、`n`、`s`、`D`……共 60 个 `custom`、63 个 `postProcess` 函数体依赖它们）。这就是为什么运行时需要“保留闭包的模块”；原生化必须在编译期把这些模块级 helper 解析并内联，而不是逐个 hook 翻译。

## 1. 阶段 1：清单与输出基线

| 编号 | 交付物 | 验收 |
| --- | --- | --- |
| 1.1 ✅ | 逐类清单 `inventory.json`：每个不同函数体一行（字段、hook 数、分类、风险、依赖、示例 id）+ 版本化 spec 未适配项 + gate | CI `classify-native-hooks --check` 通过；specs 包升级或分类器变化必须以 diff 形式被 review |
| 1.2 | **输出基线**：`crates/ec_engine/testdata/native-hooks/baseline/<field>/<bodySha256>.json`，每个函数体一份 `{ inputs[], expected[] }`。输入含 `args`、`exec` 夹具（按 `{command,args}` 精确匹配的 `stdout/stderr/status`）、`env`、`cwd` 树、`scriptTimeout`。期望值由现有 `reference-hook-worker.mjs`（Node VM、源闭包、mock exec、硬超时）采集 | `inventory.outputBaseline.coveredUniqueBodies == 594`；blocker `output-baseline-not-established` 消失；`--check` 进 CI |
| 1.3 | 六个维度的引擎级 golden（扩展 `testdata/phase1`）：候选（`name/insertValue/description/icon/priority/type/displayName/hidden/isDangerous`）、插入（`insertValue`、`{cursor}`、`getQueryTerm` 对预测 buffer 的影响）、排序（priority + frecency + acceptance）、缓存（`cache.ttl/strategy/cacheByDirectory`、`generatorArgId`、SWR 过期）、shell 环境（`custom` 的 `context.currentProcess/environmentVariables/currentWorkingDirectory/sshPrefix`）、超时（`scriptTimeout`、deadline 到期 → 空结果 + `HookDiagnostic`） | 200+ 条 buffer（git/npm/pnpm/docker/kubectl/cd/ls/brew/cargo…）在 mock exec 下的 `CompleteResult` JSON 全部锁定 |

1.2 的主要人力在 373 个 `postProcess` 函数体的**真实 stdout 样本**。做法：

- 从 IR 里每个 generator 的 `script` 取命令名，建立一次性录制的真实输出目录 `tests/fixtures/cli-output/<sha(command argv)>.txt`（在开发机上录一次，脱敏后提交）；
- 找不到真实样本的用按输出形状归类的合成样本（JSON 数组 / 行列表 / `key value` 表 / 空输出 / 非零退出）；
- 每个函数体至少 3 组输入：正常、空输出、畸形输出（触发 `catch` 分支）。

## 2. 阶段 2：类型化 IR + 原生适配

原则：**能静态表达的编成 IR，不能的写具名 Rust 适配器；两者都没有的，编译失败。**阶段 2 结束时 `counts.uniqueBodies["requires-native-adapter"] == 0`。

### 2.1 Typed IR v2（无副作用的 hook：`postProcess`、`getQueryTerm`、`trigger`、`script`、`filterTemplateSuggestions`，共 455 函数体）

在 `scripts/typed-hook-ir.mjs` / `crates/ec_engine/src/typed_hook.rs` 的封闭表达式语言上扩展，两端同步、`deny_unknown_fields`、按 UTF-16 语义：

- 值类型：`string`、`bool`、`integer`、`string[]`、`json`（`JSON.parse` 结果，带类型守卫）、`suggestion`、`suggestion[]`、`null`。
- 字符串：`split/trim/trimStart/trimEnd/replace/replaceAll/match/startsWith/endsWith/includes/slice/substring/indexOf/lastIndexOf/toLowerCase/toUpperCase/padStart/padEnd/repeat`、模板字符串、拼接。
- 正则：**只接受字面量**，编译期翻译到 Rust `fancy-regex` 语法并在编译期验证；`v` flag、命名组回引等不可翻译的 fail closed（现有 `regexp-v` 门禁保留）。
- 数组：`map/filter/flatMap/slice/join/some/every/find/findIndex/includes/indexOf/length/concat/reverse/sort`（比较器限定为 `localeCompare`/数值差/字符串比较三种形状）。
- 控制流：`if/else`、三目、`&&/||/??`、块内 `const/let` 单次赋值、提前 `return`、`try/catch → 默认值`、`for…of`（无 break 以外的副作用）。
- 建议对象：对象字面量，键限定为 `Fig.Suggestion` 已知字段；展开 `...row` 仅允许来自 `suggestion` 类型的值；未知键 fail closed。
- **模块级 helper 内联**：用 acorn + eslint-scope 把 hook 的自由变量解析到模块顶层声明；常量直接折叠，纯函数按调用点 β-归约（大小上限，递归拒绝）；引用 `fig.*`、`window`、`process`、`require`、`console` 之外任何宿主对象的 helper fail closed。这一步解决 60+63+18+13 个函数体的 `unbound-identifiers`。
- `script` 类 hook 的返回值统一成 `argv: string[]`（已有的 string → `sh -c`、`{command,args}` 两种形状在编译期归一）。

验收：这些字段全部函数体要么进 `typed-hooks.json`，要么落到 2.2 的具名适配器；`typed-ir-research-candidate` 归零（研究候选全部提升为生产 IR 或降为适配器）。

### 2.2 带副作用的 IR + 具名适配器（`custom`、`alias`、`loadSpec`、`generateSpec`，139 函数体，其中 104 调用 `exec`）

- 新增效应节点：`exec { command, args, cwd?, env?, timeout? } → { stdout, stderr, status }`；顺序 `await` 编成直线效应序列，`Promise.all` 编成并行批。Rust 侧沿用 `process::execute` 和现有的 deadline 钳制。
- 上下文访问：`context.currentWorkingDirectory/currentProcess/sshPrefix/environmentVariables[...]/searchTerm/isDangerous`（`JsHost::enter_with_context` 已经在喂这些字段，改为直接喂给 IR 求值器）。
- `generateSpec`/`loadSpec` 返回 spec 对象：IR 提供 `spec` 构造算子，Rust 复用 `merge_generated_spec`。
- 编不出来的函数体 → `crates/ec_engine/src/native_adapters.rs` 中以 `bodySha256` 为键的 Rust 实现，JS 原文只作注释参考；每个适配器必须有 1.2 基线对应的测试。预估 30–60 个（kubectl/docker/gh/npm/yarn/pnpm/git 的复杂 `custom`）。`aws`/`az` 已被 `specs.config.json` 排除，不在范围内。

### 2.3 版本化 spec 适配（消除 `versioned-spec-behaviour-unadapted`）

- 编译期：为每个版本文件按 `versions` 键逐级应用 diff（在 Node 侧移植 `@fig/autocomplete-helpers` 的 `applySpecDiff`），每个可选版本各出一份 IR；diff 里的 9 个函数按 2.1/2.2 处理。
- `index.json` 增加 `versioned: { command: ["heroku","--version"], regex, fallback, files: { "8.0.0": …, "8.6.0": … } }`；5 个 `getVersionCommand` 函数体编成 typed IR（都是 `exec` + 正则）。
- Rust `Registry`：按会话缓存版本检测结果，选择 ≤ 版本的最高文件；检测失败取最高文件（与 WebView 一致）。
- 完成后删除 `KNOWN_UNAPPLIED_VERSION_DIFFS` / `KNOWN_VERSION_SELECTORS`。

### 2.4 编译器门禁

`assertNoUnknownFunctionFields` 保留；每个抽取出的函数体必须归入 `{typed-ir, native-adapter}` 之一，否则编译失败（取代今天的 `requires-native-adapter` 桶）。`bundle/specs-ir` 停止生成 `hooks/`、`source-modules/`、`hook-modules.json`（放到阶段 4 一起删，阶段 2/3 期间双路径需要它们）。

## 3. 阶段 3：双路径比较（仅 dev/test）

| 编号 | 内容 | 验收 |
| --- | --- | --- |
| 3.1 | `ec_engine` 增加 feature `js-compat`（默认开）；`#[cfg(test)] mod dual_path` 对 1.2 基线与 1.3 golden 同时跑 typed/native 与 QuickJS，断言 JSON 逐字节相等（允许的归一化逐条写明） | 594/594 函数体、全部 golden 双路径零差异 |
| 3.2 | `ec engine complete --compare` 开发标志：真实 CLI 场景下两路都跑并打印 diff；`scripts/dual-path-session.sh` 用录制的 buffer/cwd 序列回放（T4.1 删掉 `--compare` 后改名 `scripts/replay-sessions.sh`，录制数据在 `tests/session-replay/`） | 在 git/npm/docker/kubectl/cd 五类真实仓库目录下零差异 |
| 3.3 | 终端 + GPUI 场景：用 CLAUDE.md 提到的 `remote.sock`/`desktop.sock` 驱动器回放 `EditBufferHook` + caret 帧，overlay 走 native 路径 | `fig_desktop` 测试通过；人工在 Terminal/iTerm/Ghostty/VS Code 各跑一轮无回归 |
| 3.4 | 切换条件全部满足后，把默认后端切到 native，QuickJS 留在 `js-compat` feature 后一个版本 | `inventory.gate.pathSwitchAllowed == true`，并且是 CI 强制项 |

切换条件（全部满足，且由 `classify-native-hooks --check` 计算，不是人判断）：`requires-native-adapter == 0`、`typed-ir-research-candidate == 0`、基线 594/594 在 native 路径通过、`versioned-spec-behaviour-unadapted` 消失、引擎 golden 在 native 路径通过、3.2 零差异。

## 4. 阶段 4：删除运行时 JS ✅

1. ✅ 删 `rquickjs` 依赖、`js_host.rs`、`snapshot.rs` 里的模块校验分支；编译器停止输出 `hooks/`、`source-modules/`、`hook-modules.json`；`audit-spec-hooks`/`spec-pair` 相应收紧（这三样出现即失败）。
2. ✅ 发布门槛写进 CI / release：`scripts/assert-no-runtime-js.sh`（`fig_desktop` 不链 `rquickjs`；Resources / `specs-ir` 无 `*.js`/`*.mjs`；payload ≤ 35 MiB）。`cargo test --workspace --locked` 已在 CI Rust job。
3. ✅ 文档：CLAUDE.md 的 Completion engine / Bundled Specs 段落改写；CHANGELOG 记“桌面零运行时 JS”。

## 5. 工作量与顺序

单人估算：1.2 基线语料 3–5 天；1.3 golden 2 天；2.1 typed IR v2 1.5–2 周；2.2 效应 IR + 适配器 2–3 周；2.3 版本化 2 天；阶段 3 1 周；阶段 4 2–3 天。合计约 6–8 周。顺序不可颠倒：没有 1.2 的基线，2.x 的每个适配器都无法证明等价；没有 3.4 的机器判定，阶段 4 不能开始。

## 6. 本轮核对结论（2026-09-17）

对照四阶段目标检查现有实现，发现并处理的偏差：

| 偏差 | 处理 |
| --- | --- |
| typed trigger / getQueryTerm 参考基线过期（编译器 path 排序改为 code-point 后 15 个 `gem` hook 的 provenance 变化），CI `--check` 必失败 | 重新采集两份基线并提交；CI 增加 `--get-query-term --check` |
| 版本化 spec 的 `versions` diff（15 个，含 9 个函数）和 5 个版本选择器被编译器**静默**忽略 | 建立 `KNOWN_UNAPPLIED_VERSION_DIFFS` / `KNOWN_VERSION_SELECTORS` 显式允许清单；编译器对未列出或已过期项 fail closed 并打印 `Unadapted:` 摘要；清单与 gate 计入 `versioned-spec-behaviour-unadapted` |
| 阶段 1 的“逐类清单”只存在于一次性 CLI 输出，未提交、CI 不校验 | 新增 `inventory.json` + `--check/--update`，CI 校验 |
| Node 20 下 `spec-pair.mjs` 依赖的 `JSON.parse` reviver `context.source` 不存在，99 个脚本测试假失败 | 不是代码缺陷：仓库要求 Node ≥ 22.13（`.mise.toml` 已钉 22.23.1）；本地 shell 需先 `mise` 激活 |
| CI（ubuntu）上 32 个脚本测试失败：7 个脚本把 macOS 的 `/tmp → /private/tmp`、`/var → /private/var` 当成无条件别名，Linux 下每个临时目录夹具都被判为 "escapes its approved root" | 别名表按 `process.platform === "darwin"` 门控 |
| CI Rust job 没有 `bundle/specs-ir`（gitignored、无 Node），`typed_hook` 的两个 provenance 测试读不到 `.spec-pair.json` 而失败 | Rust job 在 `cargo test` 前安装 pnpm/Node 并 `compile-spec-ir`，与 `build-app.sh` 一致，让 provenance 校验在 CI 上真正生效 |
| 受限审计子进程（Node permission model）同时授权 `bundle/specs` 与 `bundle/specs-ir` 时，Node 22.23 把前者当成文本前缀，`bundle/specs` 目录本身 readdir/lstat 被拒（文件可读）。之前只靠 macOS firmlink 换一个拼写绕过，Linux CI 上 `capture-typed-trigger-reference --check` 直接失败 | `permissionGrants` 对被同名前缀兄弟遮蔽的根改为 `<root>*` 授权（仅此一种情形），并加了同前缀兄弟目录的回归测试 |

阶段 4 已处理：`hooks/` / `source-modules/` / `hook-modules.json` 不再写出；`JsHost` 与 `rquickjs` 已删除。
