# Easy Complete 跨平台改造计划

**分支:** `feat/cross-platform`
**日期:** 2026-08-20
**状态:** 已评估、已修订。macOS 产品面保持唯一发运目标，直到本计划对应里程碑合入并有 CI 绿灯。
**依据:** 仓库现状 + 交叉编译实测 + 深度调研报告（Partial）。本文件取代口头推断。

---

## 0. 一句话结论

当前**唯一能编、能装、能发**的产品面是 **Apple Silicon macOS**。树里的 Linux / Windows 桌面后端是 GPUI 迁移前就坏掉的 fork 考古，**不能当现成实现，也不能靠切 `cfg` 复活**。跨平台必须拆成：

1. **已经可复用的底座**（IR 引擎、Unix IPC、POSIX PTY、shell hook）
2. **必须按操作系统重写的原生表面**（浮层宿主、caret、权限、托盘、开机启动、打包）

顺序是 **Linux 先行、Windows 后置**。Linux 先走通「无头引擎 → `ecterm` + shell hook → GPUI 浮层」，Windows 再补 named pipe 与 ConPTY。

---

## 1. 目标与非目标

### 1.1 产品目标（按里程碑）

| 里程碑 | 用户能得到什么 | 平台 |
| --- | --- | --- |
| **M0** | 本计划落地；非 macOS 编译失败变成 CI 红灯而不是口头结论 | 仓库 / CI |
| **M1** | `ec engine complete --buffer "git ch"` 在 Linux 上跑通（无浮层） | Linux |
| **M2** | `ecterm` + zsh/bash/fish hook 在 Linux 上驱动引擎（仍无浮层，结果可走 stdout / 日志） | Linux |
| **M3** | GPUI 补全列表跟随 caret；caret 缺失则隐藏，**不用窗口矩形兜底** | Linux（X11 + GNOME 优先） |
| **M4** | Linux 安装物（`.desktop` + 数据目录 + `specs-ir`），可卸载 | Linux |
| **M5** | Windows 上 IPC + ConPTY + 无头引擎 | Windows |
| **M6** | Windows GPUI 浮层 + 安装物 | Windows |

macOS 行为、Accessibility、IME、`.app` / DMG **全程不得回退**。每一步的回归门槛写在第 7 节。

### 1.2 非目标（写死，review 时用来否决跑偏）

- **不**重新引入 `wry` / WKWebView / `packages/autocomplete-app` / `packages/dashboard-app`。
- **不**在 caret 缺失时用窗口矩形摆放列表（比没有更糟）。`overlay.rs` 已写明：没收到 caret 就停在上次有效位置或保持隐藏。
- **不**把已删除的 `crates/fig_desktop/src/platform/{linux,windows.rs}` 考古树恢复进仓库；Linux/Windows 表面对着现行 `event.rs` / `overlay.rs` / GPUI 宿主**重写**，不要翻 `cfg` 复活。
- **不**从 v2.0.45 删掉的 universal / Linux / Windows Makefile 或 Linux 打包资源「恢复发行」。
- **不**把 IME / IBus / UI Automation 当成 v1 的硬依赖。Linux / Windows v1 先走 **PTY + 编辑缓冲**；caret 是浮层的前置，不是引擎的前置。
- **不**为了重连而去 disable/enable 输入源（macOS IME 的已知坑；别复制到别的平台）。
- **不**把 AppKit / `objc2-app-kit` / `macos-utils` 链进 `fig_util`（会拖进每个 `ecterm` 标签页）。
- **不**在本计划里重做 Intel Mac / universal 二进制。
- **不**把 Sway / 任意 Wayland compositor 列入 v1。
- **不**把 `scripts/setup.sh` 里的 WebKit/GTK/IBus 依赖表当成现行桌面构建的真源。

---

## 2. 现状（经核对）

### 2.1 发运与 CI

- 自 **v2.0.45** 起，发行管线端到端改为 ARM64-only，并删除过时的 universal / Linux / Windows Makefile 与 Linux 打包资源。
- `scripts/build-app.sh` 在非 Darwin/arm64 上直接退出。唯一发运物是 `.app`：`easy-complete`、`ec`、`ecterm`、`Contents/Helpers/EasyCompleteInputMethod.app`。
- `install.sh` / `uninstall.sh` / `make-dmg.sh` / `sign-macos-app.sh` / `notarize-dmg.sh` 只围绕该 bundle。
- `.github/workflows/ci.yml`：JavaScript 在 `ubuntu-latest`，**Rust clippy / test / dist 只在 `macos-15`**。Release 只打 ARM DMG。
- README 明确：**Platform: macOS only. The published DMG is Apple Silicon / ARM64 only.**

### 2.2 必须按 OS 重写的表面

| 表面 | 现行实现 | 跨平台含义 |
| --- | --- | --- |
| 浮层宿主 | `gpui_host.rs`：一个 `NSApplication` / `GPUIApplication` 托管 overlay + settings + tray | 抽出与 AppKit 无关的事件泵；macOS 保留 `ensure_gpui_ns_application` |
| 浮层窗口 | `ec_gpui/src/macos.rs`：`orderOut` / `orderFrontRegardless` / `setLevel` / 非激活 panel / join-all-spaces | Linux/Windows 各自实现「不抢焦点、始终在终端之上、隐藏时停车」 |
| Caret 坐标 | Quartz 顶左 → Cocoa，原点必须是 `NSScreen.screens[0]`，禁止 `mainScreen` | 统一「屏幕空间 caret + origin」；每 OS 自己填，转换错误会把列表打到另一块屏 |
| 权限 | Accessibility：`AXIsProcessTrusted*`、`AXUIElement`、`AXObserver`；安装时二进制哈希变了才 `tccutil reset` | Linux：AT-SPI / 合成器协议（v1 能避开则避开）。Windows：UIA 权限模型 |
| IME | `fig_input_method` + `ec_hitoolbox`（TIS / IMKit / HIToolbox 两个列表） | **Linux/Windows v1 不做对等 IME**。Ghostty/Kitty 类终端在那些平台另开 caret 通道 |
| 开机启动 | macOS 13+ `SMAppService`；12 为 LaunchAgent | Linux：XDG Autostart（`fig_integrations::desktop_entry` 已有草稿）。Windows：以后再说 |
| 打包 | Darwin arm64 `.app` + Sparkle | Linux：`.desktop` + 前缀布局，不是 App 包。Windows：zip/MSI，不是 DMG |

`ec_gpui`：macOS 窗口模块已 `cfg(target_os = "macos")`。Linux 走 `linux.rs`（按标题 find + X11 configure/map），Windows 走 `windows.rs`（GPUI HWND + `SetWindowPos`）；HWND/标志策略在 `windows_overlay.rs`，每 OS 可单测。考古 `platform/linux/` 与 `platform/windows.rs` **已删除，勿恢复**。

### 2.3 已可复用的底座

| 组件 | 证据 | 限制 |
| --- | --- | --- |
| `ec_engine` | IR + QuickJS hook，不经桌面进程；`ec engine complete` | `process.rs` 进程组、`filegen.rs` `getpwnam_r` 是 unix cfg；Windows 要补 |
| `fig_os_shim` | 对 Mac/Linux/Windows 建模；**本机交叉编译 `aarch64-unknown-linux-gnu` 已通过** | 只是 I/O 门面，不是桌面 |
| `fig_proto` | 依赖 `fig_util`，所以被其 Linux 编译错误挡住 | 协议本身与 OS 无关 |
| `fig_settings` | rusqlite bundled | 交叉编译需要目标 gcc；Linux CI 原生即可 |
| `fig_ipc` | Unix socket（macOS/Linux）；Windows named pipe（`windows_pipe.rs`，slug 在 `pipe_name.rs`） | accept/connect 测试是 `cfg(windows)`；retry/bind 策略每 OS 可测 |
| `figterm` PTY | macOS/Linux：`posix_openpt`；Windows：ConPTY 在 `pty/win/` | ConPTY 只在 `rust-windows` 编译；无 live I/O 测试 |
| shell hook | zsh/bash/fish rc 注入（`fig_integrations`） | `ec init` 在桌面 app 未运行时整段站起，Linux 无桌面时要想清楚 |
| `fig_integrations::desktop_entry` | XDG desktop / autostart | 可用，但安装路径仍假设旧 Fig 布局 |

### 2.4 编不过的残留（实测 + 读码）

本机 `cargo check --target aarch64-unknown-linux-gnu`：

- `fig_os_shim`：**通过**。
- `fig_util`：**失败**。
  - `directories.rs`：`use crate::linux::PACKAGE_NAME` —— 没有 `fig_util::linux` 模块。
  - `crate::consts::linux::DESKTOP_APP_WM_CLASS` —— `consts.rs` 只有 `macos`，没有 `linux`。
  - `APP_PROCESS_NAME` 只在 `macos` / `windows` 下定义，Linux 桌面入口会再踩一次。
- `fig_settings` / `ec_engine`：交叉编译卡在 `libsqlite3-sys` 找不到 `aarch64-linux-gnu-gcc`。这**不能**当成源码错误；验证必须放到 **ubuntu-latest 原生** job。

读码确认、尚未在 Linux 原生 runner 上编（考古 `platform/linux/` 与 `platform/windows.rs` **已删除，勿恢复**）：

| 位置 | 问题 |
| --- | --- |
| `fig_install` / `fig_integrations` / `ec_cli` 的 GNOME 路径 | 调用不存在的 crate `dbus::gnome_shell`（工作区里没有 `dbus` 包） |
| `fig_install/src/linux.rs` | 还用未声明的 `tar` / `zstd` |
| `ec_cli/Cargo.toml` | **无条件**依赖 `appkit-nsworkspace-bindings`，源码里 **零引用** |
| `ec_gpui` | `mod macos` 无条件 |
| `fig_input_method` / `ec_hitoolbox` | 非 macOS 可编，但是打印失败退出或空操作 |
| `scripts/setup.sh` | 编译依赖对齐 `rust-linux`：GTK 托盘、X11、Vulkan **头文件**。不装 WebKit。运行时浮层要 Vulkan ICD + X11；caret 要 IBus/AT-SPI（不写进 apt 列表） |

**Linux IBus：** 活路是 `platform/linux_caret/ibus.rs`。WebView 时代的 `platform/linux/ibus.rs` 草稿**已删除，勿恢复**。

---

## 3. 架构原则（每步 review 对照）

```
                 ┌─────────────────────────────────────┐
                 │  ec_engine  (IR + QuickJS, OS-agnostic) │
                 └─────────────────────────────────────┘
                                    ▲
           fig_ipc transport        │ CompleteRequest
     Unix socket (macOS/Linux)      │
     named pipe  (Windows, 后置)    │
                 ┌─────────────────────────────────────┐
                 │  ecterm (PTY + 编辑缓冲)              │
                 │  POSIX pty | ConPTY                   │
                 └─────────────────────────────────────┘
                                    ▲
                 ┌─────────────────────────────────────┐
                 │  OverlayHost (GPUI, 无 WebView)       │
                 │  OverlayPlatform trait:               │
                 │   - caret → 屏幕坐标                    │
                 │   - show / park（不抢焦点）              │
                 │   - 无 caret ⇒ 隐藏                    │
                 └─────────────────────────────────────┘
                    macOS │ Linux │ Windows
                    AppKit│ X11/  │ Win32/
                          │ layer │ UIA
```

1. **底座先绿，表面后写。** 先让无头 crate 在目标 OS 的 CI 上 `clippy -D warnings`，再碰 GPUI。
2. **一个 caret 契约。** 浮层只吃 `WindowPosition::RelativeToCaret { caret_position, caret_size, origin }`。新后端必须发这个，禁止再引入 `PositionRelativeToRect`。
3. **macOS 实现继续住在 `cfg(target_os = "macos")` 后面**，不要为了跨平台把 AppKit 调用摊进共用模块。
4. **`fig_desktop` 的 linux/windows 目录要么重写要么删掉**，不要「修到能选中」。
5. **进程内存约束仍然有效：** `ecterm` 按标签页翻倍；`fig_util` 禁止 AppKit；grid 的 `max_scroll_limit = 1` 不能改成 0。

---

## 4. 分步改造计划

每一步对应一个可单独 review 的 PR。合入条件见第 7 节。**不要跳步**：M3 之前没有 Linux 浮层，M1 之前没有 Linux CI。

### 阶段 A — 门禁与底座（M0 → M1）

#### PR-A1　补齐 `fig_util` 的 Linux 常量，让纯 Rust 底座在 Linux 目标上过编译

- **目标:** `fig_util` / `fig_os_shim` / `fig_proto` 在 Linux 上 `cargo check` 通过。
- **做:**
  - 在 `crates/fig_util/src/consts.rs` 增加 `linux` 模块：`PACKAGE_NAME`、`DESKTOP_APP_WM_CLASS`、`APP_PROCESS_NAME`（Linux 无 `.exe`）。
  - 修正 `directories.rs` 的 `crate::linux::PACKAGE_NAME` 引用（应走 `consts`，不要虚构 `fig_util::linux`）。
  - `resources_path()` 的 Linux 前缀用 `easy-complete`，不要沿用 `/usr/share/fig`。
- **不做:** 不定桌面后端；不改 macOS 路径；不引入 AppKit。
- **验收:**
  - 本机：`cargo check -p fig_util -p fig_os_shim -p fig_proto --target aarch64-unknown-linux-gnu`
  - macOS：`cargo test -p fig_util`、`cargo clippy --workspace -- -D warnings` 仍绿。
- **风险:** 常量字符串一旦被安装器/WM_CLASS 用上就难改；选 `easy-complete` / `dev.emmmm.easy-complete` 并在测试里钉死。

#### PR-A2　Linux CI：原生 runner 编无头 crate

- **目标:** 交叉编译缺 gcc **不再**被误当成源码结论。`fig_settings` + `ec_engine` 在 Ubuntu 上绿灯。
- **做:**
  - `.github/workflows/ci.yml` 增加 `rust-linux` job（`ubuntu-latest`）。
  - 第一刀只跑：`fig_os_shim`、`fig_util`、`fig_proto`、`fig_settings`、`ec_engine` 的 `clippy -D warnings` + `test`。
  - **不**在此 job 跑 `fig_desktop` / `ec_gpui` / `ec_cli`（现在必然红）。
- **不做:** 不把 Linux job 设成 `continue-on-error` 来掩盖失败。
- **验收:** PR 在 macos-15 **和** ubuntu 上都要绿；macOS dist 构建不受影响。
- **风险:** rusqlite bundled 在 Ubuntu 需要 `build-essential`；rquickjs 可能要 clang。写进 job 的 apt 列表，**不要**装 WebKit。

#### PR-A3　无头 CLI：`ec engine complete` 在 Linux 可用

- **目标:** 不启动桌面进程，Linux 上能对 `git ch` 给出补全。
- **做:**
  - `ec_cli`：把 `appkit-nsworkspace-bindings` 挪到 `cfg(target_os = "macos")`（源码零引用，纯减负）。
  - 给 `ec_cli` / `fig_install` / `fig_integrations` 里 `dbus::gnome_shell` 的 Linux 分支加 `cfg` 或暂用 `unimplemented` **编译桩**，让 `ec` 二进制能链上；**不要**实现 GNOME 扩展。
  - 引擎 specs 目录：Linux 用 `EC_SPECS_DIR` 或 `/usr/share/easy-complete/specs-ir`，不要读 `.app/Contents/Resources`。
  - 确认 unix 进程组 / `getpwnam_r` 在 Linux 测试下行为正确。
- **不做:** 不安装器、不做浮层、不修 doctor 的 GNOME 检查（可标 skip）。
- **验收:** Ubuntu CI：`cargo run -p ec_cli -- engine complete --buffer "git ch"` 退出 0 且 stdout 含 `checkout` 类建议；macOS 同样命令不回归。

### 阶段 B — PTY 与 shell（M2）

#### PR-B1　`fig_ipc` + `figterm` 在 Linux CI 编译并测 POSIX PTY

- **目标:** 一个 `ecterm` 进程能起来、grid 仍是 `max_scroll_limit = 1`、socket 走 `$XDG_RUNTIME_DIR/ecrun/`。
- **做:**
  - Linux CI 扩大到 `fig_ipc`、`figterm`。
  - 核对 `directories.rs` 的 runtime/socket 路径在 Linux 测试里走 XDG，而不是 `$TMPDIR` 的 macOS 形状。
  - Tokio 池保持 2 worker（标签页会翻倍）。
- **不做:** 不改 scrollback；不把 Windows named pipe 提前做进来。
- **验收:** `cargo test -p figterm -p fig_ipc` 在 Ubuntu 绿；macOS figterm 测试绿。
- **风险:** `alacritty_terminal` vendored fork 的 Linux termios 路径可能有死代码。以实际编译日志为准，不要预加依赖。

#### PR-B2　Shell 集成在无桌面 / 有桌面两种模式下可预测

- **目标:** Linux 上 `ec integrations install` 能写 zsh/bash/fish rc；桌面未运行时的 `ec init` 策略明确。
- **做:**
  - 文档化：无桌面时是「完全站起」（与现 macOS 一致）还是允许无头 hook。**建议 v1 与 macOS 一致**，避免和发行版自带补全抢。
  - 不要把 IME 安装步骤搬到 Linux。
- **验收:** 集成测试或脚本：在临时 `HOME` 里安装 hook，rc 可撤销；doctor 在无 dbus 时不 panic。
- **风险:** Otty / VS Code Terminal Suggest 共存逻辑是 macOS 上的产品决策，Linux 照抄。

### 阶段 C — GPUI 宿主去 AppKit 化（M3 前置）

#### PR-C1　`ec_gpui`：把 macOS 窗口硬化收进 `cfg`，抽出平台无关的列表

- **目标:** `ec_gpui` 在 Linux 上至少能编过 **list + theme**；overlay 窗口 API 变成 trait / cfg 分发。
- **做:**
  - `lib.rs`：`#[cfg(target_os = "macos")] mod macos;`
  - `overlay.rs` 里 `harden` / `park` / `set_overlay_frame` 改为 `platform` 模块（macOS 走现实现，其它 OS 先 `todo` 或空实现，但要能 `cargo check`）。
  - 列表、匹配、主题保持平台无关（已是）。
- **不做:** 不在这一步实现 X11 窗口；不升级 gpui，除非 check 证明 0.2.2 在 Linux 编不过。
- **验收:** Ubuntu：`cargo check -p ec_gpui`；macOS：overlay 单测 + 手测光标跟随不回归。
- **风险:** **gpui 0.2.2 在 Linux/Windows 上能否做出不抢焦点的 popup 未验证。** 若 0.2.2 的 Linux 后端不可用，升级 gpui 是单独的高风险 PR（C1b），禁止夹带功能。

#### PR-C2　`fig_desktop` 宿主：事件泵与平台后端解绑

- **目标:** `gpui_host::start_application` 在非 macOS 也能 `Application::new().run`；`ensure_gpui_ns_application` 仍仅 macOS。
- **做:**
  - `PlatformBoundEvent` 里 macOS 独有字段保持 `cfg`。
  - **删除或隔离** 编不过的 `platform/linux/*` 与 `platform/windows.rs`，换成最小 stub：`accessibility_is_enabled → None`、`get_cursor_position → None`、caret 事件 no-op。
  - stub 必须遵守：无 caret ⇒ overlay 隐藏。
- **不做:** 不在 stub 里用窗口矩形；不接 IBus；不把 tao EventLoop 请回主循环（GPUI 已是宿主）。
- **验收:** Ubuntu：`cargo check -p fig_desktop`（允许 overlay 永远隐藏）。macOS clippy workspace 绿；手测设置窗口 / 托盘 / 浮层。
- **风险:** `tao` + `tray-icon` + `muda` 在 Linux 上会拉 GTK。需要明确系统依赖，且不能把 WebKit 带回来。

### 阶段 D — Linux caret 与浮层（M3）

#### PR-D1　Caret 源（X11 优先）

- **目标:** 在 X11 终端里拿到 caret 矩形，转换成 `RelativeToCaret`，喂给现行 `overlay.rs`。
- **做:**
  - 新模块（不要在旧 `x11.rs` 上打补丁凑合）：声明 `x11rb`，跟踪焦点窗口。
  - 能从 IBus `SetCursorLocation` 得到的 caret **可以**复用其几何换算（它已经是 `RelativeToCaret`），但连接代码重写，依赖走 `Cargo.toml` 声明的 `zbus`，不要复活失踪的 `dbus` crate。
  - 无 caret：调用现有 hide/park。
- **不做:** 不做 Sway；不做「用窗口框猜 caret」；GNOME Wayland 放到 D2。
- **验收:** 手测至少：GNOME 终端或 kitty（X11 会话）、外接屏不要错位（原点策略写进代码注释，对标 macOS `screens[0]` 的教训）。自动化：几何换算单测。

#### PR-D2　Wayland（GNOME）caret —— 仅在 D1 稳定之后

- **目标:** GNOME Wayland 下浮层可用。
- **做:** 评估 layer-shell + 终端专有协议；**GNOME Shell 扩展不是 v1 必选项**（旧路径依赖失踪的 `dbus` crate 和已删打包资源）。
- **不做:** 不为「所有 Wayland」承诺同一实现。
- **验收:** 一台 GNOME Wayland 机器上手测；caret 丢失时列表隐藏。

#### PR-D3　Linux 浮层窗口行为

- **目标:** 不抢焦点、终端全屏时仍可见或按策略隐藏、hide 时 park 而不是销毁（保持上次尺寸，对标 macOS `orderOut`）。
- **验收:** 与 macOS 相同的隐藏触发：整段删 token、粘贴 / 调历史 → `HIDDEN_UNTIL_KEYPRESS`；自己插入豁免。这些逻辑已在 `overlay.rs`，平台层不得绕过。

### 阶段 E — Linux 安装物（M4）

#### PR-E1　布局与集成

- 前缀：`/usr/bin/ec`、`/usr/bin/ecterm`、`/usr/bin/easy-complete`（或 `/usr/libexec`）、`/usr/share/easy-complete/specs-ir`。
- `.desktop` + 可选 XDG autostart（已有 `desktop_entry.rs`）。
- 卸载对称。不要 `tccutil`，不要 IME symlink。

#### PR-E2　打包

- 先 tar.gz / 自描述目录，再考虑 deb。
- **禁止**把 `setup.sh` 的 WebKit 依赖写进包装。
- `build-app.sh` 保持 Darwin-only；另开 `scripts/build-linux.sh`，不要把 `.app` 逻辑 `if linux`。

### 阶段 F — Windows（M5 → M6，后置）

排在 Linux M3 之后。原因：IPC 是 `compile_error!`、引擎 unix-only 路径、桌面后端类型已经对不上。

| PR | 内容 |
| --- | --- |
| F1 | `fig_ipc` named pipe transport；公共 trait，Unix 实现保持现状 |
| F2 | `ec_engine` Windows 进程终止 / 无 `getpwnam` 的 `~user` |
| F3 | `figterm` ConPTY 在 windows-latest CI 编译 |
| F4 | **重写** caret（UIA），对接 `RelativeToCaret`；**删除** `PositionRelativeToRect` 残留，而不是让它重新编译 |
| F5 | GPUI 浮层 + 不抢焦点 |
| F6 | 安装物（zip 先于 MSI） |

Windows CI 用 `windows-latest`，与 macos / ubuntu 并列，互不 `continue-on-error`。

---

## 5. 建议的文件落点（按阶段）

| 阶段 | 主要文件 |
| --- | --- |
| A1 | `crates/fig_util/src/consts.rs`, `directories.rs` |
| A2 | `.github/workflows/ci.yml` |
| A3 | `crates/ec_cli/Cargo.toml`, `crates/fig_install/src/linux.rs`, `crates/ec_engine/src/{worker,process,filegen}.rs` |
| B1 | `crates/fig_ipc/src/unix_socket.rs`, `crates/figterm/src/pty/unix.rs`, `crates/fig_util/src/directories.rs` |
| B2 | `crates/fig_integrations/src/**`, `crates/ec_cli/src/cli/integrations.rs` |
| C1 | `crates/ec_gpui/src/{lib,overlay,macos}.rs`（新增 `platform/`） |
| C2 | `crates/fig_desktop/src/{gpui_host,platform/mod.rs}`；隔离或删除 `platform/linux/*`, `platform/windows.rs` |
| D1–D3 | **新建** `crates/fig_desktop/src/platform/linux_caret/` 或同等；`overlay.rs` 只消费 caret |
| E | `scripts/build-linux.sh`（新）, `crates/fig_integrations/src/desktop_entry.rs` |
| F | `crates/fig_ipc` 新 transport, `crates/figterm/src/pty/win/`, 新 Windows caret 模块 |

---

## 6. 计划评估与修订记录

调研原稿建议「先让 `fig_desktop` 的 linux/windows 分支在现行 GPUI 事件模型下编过」。评估后**否决作为第一步**，原因：

1. **类型已经对不上。** Windows 后端引用的 `RelativeDirection` / `FigWindowMap` / `PositionRelativeToRect` 在现行 `event.rs` 里不存在。把它「编过」等于在过时 API 上施工，下一步还得拆掉。
2. **依赖不在清单里。** Linux 后端要的 `x11rb`/`zbus`/`dbus` 未声明，且工作区没有 `dbus` crate。补依赖让考古文件通过 clippy，会把死代码变成 CI 负担。
3. **底座自己就编不过。** 实测 `fig_util` 缺 Linux 常量，`fig_proto` 被它拖死。桌面前必须先修这个。
4. **IBus 草稿比调研说的新一点。** 它已发 `RelativeToCaret`，但仍然不是 GPUI 路径，也不能编译。修订：D1 可以**借鉴几何**，不可以「打开 cfg」。
5. **交叉编译不是证据。** sqlite 缺 `aarch64-linux-gnu-gcc` 只说明本机无交叉工具链。A2 必须用 Ubuntu 原生 job。
6. **Linux 先行是现写进本文件的决策**，不是仓库里已有的官方顺序。依据是 POSIX PTY + Unix socket 已存在，Windows 要先长 IPC。
7. **`setup.sh` 的 WebKit 包是错误路标。** 现行 UI 是 GPUI。E 阶段的系统依赖从 GPUI/GTK/X11 重新列，不从 setup.sh 抄。
8. **`local_webview_data_dir` 这种名字不带进 Linux 安装布局。** 那是 WebView 时代的数据目录。**已关（2026-08-23）：** 函数已删；Linux uninstall 只清 `fig_data_dir`（`easy-complete` 前缀），不再扫 `q-desktop` 一类 webview 旁路。

**仍开放、必须用后续 PR 关闭的问题：**

- ~~gpui 0.2.2 能否在 Linux 上承载不激活 popup~~ **已关（2026-08-23）。** `WindowKind::PopUp` + `_NET_WM_WINDOW_TYPE_NOTIFICATION` + `_NET_WM_STATE_ABOVE`（map 之后再发 `_NET_WM_STATE` ClientMessage）。本机 `DISPLAY=:2` xfwm4：`Fig Autocomplete` 是 NOTIFICATION/ABOVE，`_NET_ACTIVE_WINDOW` 不是它。不必为这个升级 gpui。
- Ubuntu 原生 runner 上 `ec_gpui` / `fig_desktop` 的第一次编译日志（C2；本机无 `aarch64-linux-gnu-gcc`，不能用交叉编译当证据）。
- GNOME Wayland 有没有不依赖 Shell 扩展的 caret（D2；失败则 v1 只支持 X11 / XWayland + IBus）。
- 无桌面 Linux 是否提供「纯 CLI 补全」（建议否，与 macOS `suppress_without_desktop_app` 对齐）。

---

## 7. 每一步的执行协议（plan → 做 → review → fix）

对每一个 PR：

1. **Plan 模式。** 对照本文件该节的「做 / 不做 / 验收」，列出将改的文件。若要改非目标里的东西，先改本计划再动代码。
2. **实现。** 只做该节范围。
3. **Review（对齐目标，不是对齐能否编过考古文件）。** 清单：
   - [ ] 有没有重新引入 WebView / wry / WKWebView？
   - [ ] 无 caret 时有没有用窗口矩形摆放？
   - [ ] 有没有把 AppKit 链进 `fig_util` / `ecterm`？
   - [ ] macOS：`cargo clippy --locked --workspace -- -D warnings` 与相关 `cargo test` 绿？
   - [ ] 新平台：本节列出的 crate 在对应 CI runner 绿？
   - [ ] `ecterm` 的 scrollback / tokio worker 约束还在吗？
   - [ ] 安装/IME 相关改动有没有违反「只 enable、按哈希决定是否换进程」？（macOS 回归）
4. **Fix。** Review 失败不得把里程碑标完成。
5. **更新本文件。** 该节标 `done` + 合入 SHA；开放问题关闭或下移。

macOS 回归是否决项。跨平台进度慢可以接受，把 Otty caret / 外接屏 / `···` 看门狗弄坏不可以。

---

## 8. 当前执行指针

- 分支：`fix/cross-platform-audit-1` @ `d114d993`（从 `feat/cross-platform` 切出；`feat/cross-platform` 来自 `main` @ `55b043ff`）
- **发运：** 仍只有 macOS Apple Silicon DMG。Linux / Windows 是 WIP，不是产品（无 Linux 包、无 Windows 安装器）。
- 本文件就是 M0 的文档交付物
- 2026-08-23 审计：本机 Linux（x86_64）已验证无头 crate + `ec_gpui`/`fig_desktop` `clippy -D warnings`；修了 `fig_util` HRTB、`ec_gpui` X11 API；README 标明跨平台未发运
- 2026-08-23：`rust-toolchain.toml` 去掉额外 `targets`（避免 Linux/Windows CI 拉 Darwin std）；`rust-linux` **不**装 `shellcheck`（与 macOS job 不同），测试在二进制缺失时 skip；F2 `taskkill /T` + `~user`、F3 `TerminateProcess` BOOL、F4 Win32 caret 换算可在 Linux 单测；`setup.sh` 不再装 WebKit / 不再 `rustup default stable`
- 2026-08-23 续：F5 `SetWindowPos` 策略（不抢焦点、NOSIZE、park=HIDE+NOMOVE、place 顶左取整、缺 HWND / 空虚拟屏则不摆）在 `ec_gpui::windows_overlay` 钉死，`windows.rs` 仍 `cfg(windows)`。named-pipe retry/bind 策略在 `fig_ipc::windows_pipe_policy` 钉死；accept/connect 仍是 `cfg(windows)`。F3 续：ConPTY `HRESULT` 成功是 0，与 `TerminateProcess` BOOL 相反。`rust-windows` 与 `rust-linux` **同一 crate 列表**，是 MSVC 下 ConPTY / named pipe / GPUI HWND 的**编译**，不是桌面会话——GetGUIThreadInfo、对真实 HWND 的 `SetWindowPos`、ConPTY I/O 都没有测。两 job 都在 YAML 里，**第一次 GitHub 原生 run 仍待 push**。
- 2026-08-23 续 2（本机 Ubuntu `DISPLAY=:2`）：`scripts/build-linux.sh` **真的打出了** `dist/linux/easy-complete-2.2.2-x86_64.tar.gz`（`--locked`，IR 840/734/1480）。`install-linux.sh --prefix` 装进临时目录后，前缀里的 `ec engine complete --buffer "git ch"` 含 `checkout`（不设 `EC_SPECS_DIR`）；`--uninstall` 只清该前缀，**不会**删掉指向别的 PREFIX 的 autostart。D3 浮层：无 Vulkan ICD 时 GPUI 0.2.2 在创建窗口前 panic（`NoSupportedDeviceFound`）；装 lavapipe 后进程能住，`--no-dashboard` 且**不注入 caret** 时 xwininfo/xdotool 看不到 `Fig Autocomplete`（浮层只在 show 时才建窗）。没有用窗口矩形兜底。tarball / README 标明 Linux 未发运。
- 2026-08-23 续 3（`DISPLAY=:2` 真终端 caret）：IBus 1.5.32 私有总线没有 `org.freedesktop.DBus.Monitoring`，监听改为 BecomeMonitor 失败则 `AddMatch` `eavesdrop='true'`（与 `dbus-monitor` 相同）。at-spi2 2.56 的 `Name` / `CaretOffset` / `Parent` 走属性，方法作回退。D2：X11 分类终端只在 IBus **已经订阅**时让 AT-SPI 让路。手测：`GTK_IM_MODULE=ibus` 的 xfce4-terminal → `bash (ecterm)` → `git ch`，`Fig Autocomplete` **IsViewable**，再打一个字母浮层 X +10（字符宽）。无窗口矩形兜底。GNOME Wayland 上的 AT-SPI 路径未手测。
- 2026-08-23 续 4（M2 + D3）：泛 `CI=true` **不再**挡住 wrap（只拦 `GITHUB_ACTIONS` / `Q_CI`；不要把 `Q_FORCE_FIGTERM_LAUNCH` 写进 rc）。本机 `ecterm` + bash/zsh/fish OSC 697 钩子把 `git ch` 送到 mock `remote.sock`，引擎 stdout 含 `checkout`（无 caret、无浮层）。D3：map 后再发 `_NET_WM_STATE_ABOVE`；`ec-overlay-spike` 在 `:2` 上 `_NET_WM_WINDOW_TYPE_NOTIFICATION` + ABOVE，且不是 `_NET_ACTIVE_WINDOW`。`setup.sh` 仍只装编译依赖（GTK 托盘 / X11 / Vulkan 头），不装 WebKit；caret 运行时依赖 IBus/AT-SPI 不写进 apt。
- 2026-08-23 续 5（`0ddd0de2` 审计收口）：`platform/mod.rs` 活路是 macos / linux_caret / windows_caret / stub。考古 `platform/linux/**` 与 `platform/windows.rs` **已删除，勿恢复、勿翻 cfg**。`local_webview_data_dir` 已去掉；Linux uninstall 只清 `easy-complete` 数据前缀。figterm history SQLite 走 `spawn_blocking`。desktop IPC `send_event` 关闭环时 warn 而不是 unwrap。`get_current_buffer` 原地 truncate。同一次 complete 快照 `AutocompleteFlags`。非 Mac 屏幕列表改名 `overlay_screens`。`GLOBAL_PROXY.set` / `spawn_engine` 失败走可诊断退出。本机未测 macOS AX/IME/DMG，未假装 Windows live runtime。
- 2026-08-23 续 6（`54dee500`）：本机 Ubuntu 上 `cargo test -p fig_desktop` 全绿（115 + 1 ignored）。AppImage autostart 走 `set_enabled_in(ctx)`：AppImage 链到本地 desktop entry（不是 FUSE `current_exe`），前缀安装写 `--is-startup` 文件；`app.launchOnStartup` 仍是门闩。设置权限门在非 macOS 只看 shell integration，不因 Accessibility/IME 卡住。figterm mutex/rwlock poison 恢复；`UnixTerminal::Drop` warn 而不是 unwrap；history SQLite 改独立 `std::thread`（不再用 forever `spawn_blocking` 占 blocking pool）。去掉空的 `WryIdMap` / JS handler channel。`webview/` 模块名未改（rename churn）。本机未测 macOS AX/IME/DMG，未假装 Windows live runtime。Linux tarball 已按变更二进制重打。
- 2026-08-23 续 7（`8ba16257`）：P1-2 已关。`fig_desktop/src/webview/` 改名为 `bootstrap/`，`WebviewManager` → `AppRuntime`，通知状态去掉 WebView 前缀。行为不变。`fig_desktop_api` 仍由 `fig_desktop` 链接（macOS 发运），与 `ec_overlay_spike` 都**不**在 `default-members`；dist 脚本不打 spike。本机 `cargo test -p fig_desktop` 116 + 1 ignored。本机未测 macOS AX/IME/DMG，未假装 Windows live runtime。Linux tarball 已按变更二进制重打。
- 2026-08-23 续 8（review `8ba16257`）：干净，无代码修复。无残留 `crate::webview` / `WebviewManager` / `WebviewNotificationsState`；`src/webview/` 不存在。macOS 菜单 / ActivationPolicy / Sparkle / GPUI host 只改了类型名。留下的 “webview” 字样不是 bootstrap 模块：Sparkle `show_webview`（弹更新 UI）、`ec debug devtools` 帮助文案、以及对照旧 WebView 行为的单测名。本机 `cargo test -p fig_desktop` 116 + 1 ignored；`fig_desktop_api` 3 passed；`clippy -D warnings` on fig_desktop / fig_desktop_api / fig_util / ec_overlay_spike 绿；`ec engine complete --buffer "git ch"` 含 `checkout`；`cargo fmt --all -- --check` 绿。未测 macOS AX/IME/DMG，未假装 Windows live runtime。未开 remote_ipc unwrap / tokenize / X11 cache。
- 2026-08-23 续 9（`719377b3`）：R2-4 / R2-5 / R2-3。`remote_ipc` 关闭环走 `send_event_or_warn`，protobuf encode 失败 `error!` + continue，不再 naked unwrap（`rg 'send_event\(.*unwrap' crates/fig_desktop` 只剩测试）。`figterm` `EventHandler` 的 `main_loop_sender` 与 socket/history 一样 `error!`。`Engine::complete` 只 `tokenize` 一次，把 `(tokens, ends_with_space, buffer)` 传给 `lookup::complete`，ranking root 复用已解析 token。未开 X11 cache / history OnceLock / README / overlay title。本机 `cargo clippy --offline -p fig_desktop -p figterm -p ec_engine -- -D warnings` 绿；`cargo test --offline -p fig_desktop` 117 + 1 ignored；`figterm` 37 + 1 ignored + cli 1 + linux_shell_hooks 3；`ec_engine` 212；`ec engine complete --buffer "git ch"` 含 `checkout`。未测 macOS AX/IME/DMG，未假装 Windows live runtime。
- 2026-08-23 续 10（review `719377b3`）：干净，无代码修复。`edit_buffer` 订阅循环 encode 失败 `error!`+continue，关闭环 `send_event_or_warn`；同函数 `GpuiOverlayBuffer` / `PlatformBoundEvent` 以及 `broadcast_notification_all` 仍是 `send_event()?` / `encode?`（Result，不是 panic）。`EventHandler` Prompt/PreExec 关闭 `main_loop_sender` 走 `error!`，`closed_main_loop_sender_does_not_panic` 覆盖；`figterm/src/ipc.rs` SSH flush 仍有 `main_loop_sender.send(...).unwrap()`，未开。`Engine::complete` 对 `completion_buffer` 只 `tokenize` 一次再传入 lookup；`tokens.first()` 与 overlay 记 acceptance 用的 `ranking_root_command` 同式。源码钉之外有 `chained_buffer_ranks_and_merges_history` 行为钉。本机 `cargo clippy --offline -p fig_desktop -p figterm -p ec_engine -- -D warnings` 绿；`cargo test --offline -p fig_desktop` 117 + 1 ignored；`figterm` 37 + 1 ignored + cli 1 + linux_shell_hooks 3；`ec_engine` 212；`ec engine complete --buffer "git ch"` 含 `checkout`。未测 macOS AX/IME/DMG，未假装 Windows live runtime。未开 X11 cache / history OnceLock / README / overlay title。
- 2026-08-23 续 11（`d010ebb5`）：R2-1 / R2-2 / SSH flush unwrap。`ec_gpui/src/linux.rs` 缓存 `(conn, screen_num)`，失败重试与 `overlay_screens` 共用 500ms TTL；place/probe 只留一处 `RustConnection::connect`。`apply_position` 取一次屏幕列表传给 `layout_overlay` / `overlay_bounds`，不用窗口矩形。figterm history `OnceLock` 单队列单线程，调用方 clone Sender，`history_thread_builder_runs_once` 钉 Builder 只跑一次；`TERM_SCROLLBACK_LINES` 仍为 1。`ipc.rs` SSH flush 关闭 `main_loop_sender` 走 `error!`，与 EventHandler 相同。未开 README 分支名 / 浮层标题。本机 `cargo clippy --offline -p ec_gpui -p fig_desktop -p figterm -- -D warnings` 绿；`cargo test --offline`：ec_gpui 72；fig_desktop 118 + 1 ignored；figterm 40 + 1 ignored + cli 1 + linux_shell_hooks 3；`ec engine complete --buffer "git ch"` 含 `checkout`。未测 macOS AX/IME/DMG，未假装 Windows live runtime。
- 2026-08-23 续 12（review `d010ebb5` → `b332d9ca`）：X11 缓存缺 flush / 死连接不丢 / 空 screens TTL 盖住已连上的 display；history OnceLock 在 spawn 失败时会存断线 Sender。`with_x11` 回调后 `flush`，失败 `discard_display`；成功 connect 清 screens；空列表且已有 display 则重查。history spawn `expect`（OnceLock panic 可重试），测试钉 `receiver_count > 0`。SSH flush / `TERM_SCROLLBACK_LINES == 1` / 无窗口矩形 caret / 单处 `RustConnection::connect` 保持。未开 README 分支名 / 浮层标题。本机 `cargo clippy --offline -p ec_gpui -p fig_desktop -p figterm -- -D warnings` 绿；`cargo test --offline`：ec_gpui 72；fig_desktop 118 + 1 ignored；figterm 40 + 1 ignored + cli 1 + linux_shell_hooks 3；`ec engine complete --buffer "git ch"` 含 `checkout`。未测 macOS AX/IME/DMG，未假装 Windows live runtime。
- 2026-08-23 续 13（`36a322ea`）：N1 / N2 / N5 / N4。README 与 README.zh-CN 的 WIP 分支改为 `fix/cross-platform-audit-1`；仍写明无 Linux 包、无 Windows 安装器、CI 只是门禁。浮层标题 `Fig Autocomplete` → `Easy Complete`（`ec_gpui::OVERLAY_WINDOW_TITLE`，Linux 按标题 find 用同一常量；`AUTOCOMPLETE_WINDOW_TITLE` 别名，钉死不含 Fig）。`show_webview` → `show_updater`（Sparkle 提示，不是 WKWebView）。非 Mac `quartz_y_to_cocoa_frame_y` → `screen_y_to_frame_y`；`caret_y_in_quartz_space` → `caret_y_in_screen_space`；macOS 仍保留 Cocoa `quartz_y_to_cocoa_frame_y`。补全行为 / caret 策略 / scrollback=1 / 考古后端未动。本机 `cargo clippy --offline -p ec_gpui -p fig_desktop -- -D warnings` 绿；`cargo test --offline`：ec_gpui 75；fig_desktop 118 + 1 ignored；`ec engine complete --buffer "git ch"` 含 `checkout`。未测 macOS AX/IME/DMG，未假装 Windows live runtime。
- 2026-08-23 续 14（review `36a322ea`）：干净，无代码修复。Linux place/park/harden 全部走 `OVERLAY_WINDOW_TITLE`（`"Easy Complete"`，不含 Fig）；设置窗仍是 `"Settings"`，find-by-title 不会撞。Windows 不按标题找（`windows_titled_overlay_places() == false`），HWND 标题由同一常量 `set_window_title`。Y 公式未改：`screen_y_to_frame_y` / macOS `quartz_y_to_cocoa_frame_y` 仍是 `origin+height-y-h`（100/140/0/900 → 660）；Linux ConfigureWindow 与 Win32 `SetWindowPos` 仍用顶左，不套 Cocoa 翻转；`caret_y_in_screen_space` BottomLeft 120/18/900 → 762。`show_updater` 无 `show_webview`；`src/webview/` 不存在；剩下的 webview 字样是钉「旧名已走」或对照旧行为的单测。`Failed to open` / `has started` 用 `PRODUCT_NAME`。补全 / caret 策略 / `TERM_SCROLLBACK_LINES == 1` / 考古后端未动。N3（`matches_webview` 测名）未开。本机 `cargo clippy --offline -p ec_gpui -p fig_desktop -- -D warnings` 绿；`cargo test --offline`：ec_gpui 75；fig_desktop 118 + 1 ignored；`cargo fmt --all -- --check` 绿；`ec engine complete --buffer "git ch"` 含 `checkout`。未测 macOS AX/IME/DMG，未假装 Windows live runtime。
- 2026-08-23 续 15（`d114d993`）：N3 + tokio + 用户可见 Fig/WebView 扫尾。`ec_engine` 测名 `matches_webview` / `like_the_webview` 改为 native 描述（`clean_output_strips_cr_ansi_and_blank_lines`、`script_timeout_default_and_max_priority`、`split_on_wins_over_post_process`、`omitted_priority_defaults_to_fifty`、`string_query_term_uses_separator_suffix`；`execute_command_maps_command_args_cwd_env_timeout` 一并改）；行为未动，`js_host_generate_runtime_tests_do_not_use_webview_fn_names` 钉旧名不再出现。工作区 `tokio` 从 `full` 收成 `fs, io-std, io-util, macros, net, process, rt, rt-multi-thread, signal, sync, time`（去掉 parking_lot 后端；`net` 覆盖 Unix socket 与 Windows named pipe，`signal` 覆盖 SIGWINCH / ctrl_c）。CLI：`ec debug devtools` 不再说 webview，`debug build` 不再说 Fig.js；`fig update` / `fig settings` / `fig launch` / `fig issue` 提示改走 `CLI_BINARY_NAME`；ecterm logger / IME 非 macOS stub / uninstall warn 用 `PRODUCT_NAME`。网站 How-it-works：GPUI 浮层+设置，crate `ec_cli`（不再 wry WebView / `q_cli`）。补全 / caret 策略 / `TERM_SCROLLBACK_LINES == 1` / 考古后端未动。本机 Linux `cargo clippy --locked --offline -p ec_engine -p figterm -p fig_desktop -p fig_ipc -p fig_install -p ec_cli -p fig_input_method -- -D warnings` 绿；`cargo test --locked --offline`：ec_engine 213；fig_desktop 118 + 1 ignored；figterm 40 + 1 ignored + cli 1 + linux_shell_hooks 3；fig_ipc 16；fig_install 15；ec_cli 28 + 4 ignored + cli 4 + debug 1 + internal 9 + settings 1 + init 20 + integrations 4；`cargo fmt --all -- --check` 绿；`ec engine complete --buffer "git ch"` 含 `checkout`。Darwin 交叉编 `fig_ipc`/`ec_engine` 卡在本机 cc 不认 `-arch`（sqlite），不是源码红。无 `x86_64-pc-windows-msvc` target，未假装 Windows live。网站 `node_modules` 不在，未跑 `pnpm build`。
- 2026-08-23 续 16（review `d114d993`）：干净，无代码修复。N3：`matches_webview` / `like_the_webview` 不再是活测名（`cargo test -p ec_engine -- --list` 只剩钉 `js_host_generate_runtime_tests_do_not_use_webview_fn_names` 与 ranking 钉 `match_buckets_follow_webview_exact_prefix_order`）；钉用 `join("_")` 所以 `include_str` 自身不含旧名；六个测体未改。Tokio：1.50 `default = []`，工作区列表即 `full` 去掉 `parking_lot`；`net` 覆盖 `UnixListener`/`UnixStream`、Windows `named_pipe`（tokio 还拉 `Win32_System_Pipes`）、`AsyncFd`；`signal` 覆盖 SIGWINCH / `ctrl_c`（Windows `Win32_System_Console`）；`io-std` 覆盖 stdin/stdout；figterm 仍 `new_multi_thread().worker_threads(2).enable_all()`。无 `time::pause`，不补 `test-util`，不恢复 `full`。CLI 提示走 `CLI_BINARY_NAME`（`ec`）；ecterm / uninstall 走 `PRODUCT_NAME`；IME 非 macOS stub 硬编码 `Easy Complete`（该 crate 不链 `fig_util`）。网站 How-it-works 无 wry/WebView/`q_cli`；`/fig-alternative` 遗产营销保留。补全 / caret 策略 / `TERM_SCROLLBACK_LINES == 1` / 考古后端未动。预存在：overlay 对 `WindowEvent::Devtools` 仍 ignore（帮助已不说 webview），本轮不接。本机 Linux `cargo clippy --locked --offline -p ec_engine -p figterm -p fig_desktop -p fig_ipc -p fig_install -p ec_cli -p fig_input_method -- -D warnings` 绿；`cargo test --locked --offline`：ec_engine 213；fig_desktop 118 + 1 ignored；figterm 40 + 1 ignored + cli 1 + linux_shell_hooks 3；fig_ipc 16；fig_install 15；ec_cli 28 + 4 ignored + cli 4 + debug 1 + internal 9 + settings 1 + init 20 + integrations 4；`cargo fmt --all -- --check` 绿；`ec engine complete --buffer "git ch"` 含 `checkout`。未测 macOS AX/IME/DMG，未假装 Windows live runtime。未开 Suggestion-clone / spike isolation。
- 下一步：push 后看第一次 `rust-linux` / `rust-windows`；Windows 手测 named pipe 往返、ConPTY、caret、HWND。不要为了翻 skip 去给 Ubuntu 装 shellcheck，不要装 WebKit，不要把 `mesa-vulkan-drivers` 写进 rust-linux job，**不要复活考古 `platform/linux/` / `platform/windows.rs`**，不要把 `webview/` 目录名请回来。N1 / N2 / N3 / N4 / N5 已关。

进度勾选：

- [x] M0 文档（本文件，已经过评估修订）
- [x] PR-A1（`fig_util` Linux 常量；`fig_util` / `fig_os_shim` / `fig_proto` 在 `aarch64-unknown-linux-gnu` 上 `cargo check` 通过；macOS workspace clippy 绿）
- [x] PR-A2（`.github/workflows/ci.yml` 增加 `rust-linux`；不装 WebKit；首次 GitHub 原生 run 仍待推送后验证）
- [x] PR-A3（切断 `dbus` 编译图；`ec engine complete --buffer "git ch"` 本机成功含 `checkout`；Linux CI 扩到 `ec_cli`）
- [x] PR-B1（`fig_ipc` Linux 目标 `cargo check` 过；`figterm` scrollback 钉死为 1、tokio 2 worker 未改；socket 路径测试 + Linux CI 扩到 `fig_ipc`/`figterm`）
- [x] PR-B2（无桌面 `ec init` 与 macOS 同策略；zsh hook 在临时 HOME 可装可卸；IME 不进入 Linux `integrations install all`）
- [x] PR-C1（`ec_gpui` 的 AppKit 模块 `cfg(macos)`；非 Mac 走 `platform_stub`，`overlay_screens` 为空所以无 caret 则无法摆放；本机交叉编译卡在缺 linux gcc，Ubuntu 原生 gpui 仍待 CI）
- [x] PR-C2（`fig_desktop` 非 macOS 走 `platform/stub.rs`：`accessibility_is_enabled → None`、`get_cursor_position → None`、无 caret 则 park；考古 `linux/` 与 `windows.rs` **已删除，勿恢复**；GNOME/IBus 安装路径切断；Linux CI 扩到 `ec_gpui`/`fig_desktop` clippy，系统依赖是 GTK/X11/Vulkan **不含 WebKit**。Ubuntu 原生 gpui 首次绿灯仍待推送后验证）
- [x] PR-D1（新建 `platform/linux_caret/`：X11 焦点跟踪 + zbus IBus；几何换算在 `platform/caret.rs` 单测钉死；无 caret 不摆放；旧 `platform/linux/` **已删除，勿恢复**）
- [x] PR-D2（GPUI 0.2.2 无 layer-shell，浮层仍走 X11/XWayland。GNOME Wayland 终端 caret 走 AT-SPI `GetCharacterExtents(SCREEN)`，不用 Shell 扩展；窗口 `GetExtents` 只给 IBus relative 当原点，不当列表位置。无 a11y 总线或非终端 focus 则隐藏）
- [x] PR-D3（`ec_gpui/src/linux.rs`：按标题找 overlay，`unmap` park、`configure`+`map` 显示；启动时若有 `DISPLAY` 则清掉 `WAYLAND_DISPLAY` 让 GPUI 走 X11，`EC_GPUI_BACKEND=wayland` 可退出；无屏幕列表则 park，不用窗口矩形当 edges。`linux_overlay` 钉死 NOTIFICATION + ABOVE、不发 `_NET_ACTIVE_WINDOW`；map 后再 ClientMessage。本机 xfwm4 手测浮层不是 `_NET_ACTIVE_WINDOW`。）
- [x] PR-E1 / PR-E2（`scripts/build-linux.sh` 前缀布局 + tar.gz；`scripts/install-linux.sh --prefix`；`.desktop` + hicolor 图标；不装 WebKit，不改 `build-app.sh`）
- [ ] 阶段 F（进行中。F1–F6 代码在树：slug / `~user` / BOOL+HRESULT / caret 换算 / SetWindowPos 策略 / zip 布局可在 Linux 单测。Live named-pipe accept、ConPTY I/O、GetGUIThreadInfo、HWND 仍要 Windows 主机。CI 待 push。）
