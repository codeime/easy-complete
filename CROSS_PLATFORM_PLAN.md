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
- **不**把 `crates/fig_desktop/src/platform/{linux,windows.rs}` 当可修复的现成后端；要对着现行 `event.rs` / `overlay.rs` / GPUI 宿主**重写**。
- **不**从 v2.0.45 删掉的 universal / Linux / Windows Makefile 或 Linux 打包资源「恢复发行」。
- **不**把 IME / IBus / UI Automation 当成 v1 的硬依赖。Linux / Windows v1 先走 **PTY + 编辑缓冲**；caret 是浮层的前置，不是引擎的前置。
- **不**为了重连而去 disable/enable 输入源（macOS IME 的已知坑；别复制到别的平台）。
- **不**把 AppKit / `objc2-app-kit` / `macos-utils` 链进 `fig_util`（会拖进每个 `ecterm` 标签页）。
- **不**在本计划里重做 Intel Mac / universal 二进制。
- **不**把 Sway / 任意 Wayland compositor 列入 v1（`linux/mod.rs` 已标 Sway “Not supported”）。
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

`ec_gpui/src/lib.rs` **无条件** `mod macos`，`overlay.rs` 无条件 `use crate::macos::{...}`。Linux 目标上这个 crate 现在编不过。

### 2.3 已可复用的底座

| 组件 | 证据 | 限制 |
| --- | --- | --- |
| `ec_engine` | IR + QuickJS hook，不经桌面进程；`ec engine complete` | `process.rs` 进程组、`filegen.rs` `getpwnam_r` 是 unix cfg；Windows 要补 |
| `fig_os_shim` | 对 Mac/Linux/Windows 建模；**本机交叉编译 `aarch64-unknown-linux-gnu` 已通过** | 只是 I/O 门面，不是桌面 |
| `fig_proto` | 依赖 `fig_util`，所以被其 Linux 编译错误挡住 | 协议本身与 OS 无关 |
| `fig_settings` | rusqlite bundled | 交叉编译需要目标 gcc；Linux CI 原生即可 |
| `fig_ipc` | `tokio::net::UnixStream`，Linux 可复用 | 非 Unix 是 `compile_error!`，没有 named pipe |
| `figterm` PTY | macOS/Linux：`posix_openpt`；Windows：ConPTY 目录已在 `pty/win/` | Linux/Windows 是否编过 **未在 CI 验证** |
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

读码确认、尚未在 Linux 原生 runner 上编：

| 位置 | 问题 |
| --- | --- |
| `fig_desktop/src/platform/windows.rs` | 导入已删除的 `RelativeDirection`、`FigWindowMap`，发送不存在的 `WindowEvent::PositionRelativeToRect`。现行 API 是 `FigIdMap` + `WindowPosition::RelativeToCaret` |
| `fig_desktop/src/platform/linux/` | 依赖未声明的 `x11rb`、`zbus`、`dbus`；`fig_desktop` 的 Linux 依赖只有 `sysinfo` |
| `fig_install` / `fig_integrations` / `ec_cli` 的 GNOME 路径 | 调用不存在的 crate `dbus::gnome_shell`（工作区里没有 `dbus` 包） |
| `fig_install/src/linux.rs` | 还用未声明的 `tar` / `zstd` |
| `ec_cli/Cargo.toml` | **无条件**依赖 `appkit-nsworkspace-bindings`，源码里 **零引用** |
| `ec_gpui` | `mod macos` 无条件 |
| `fig_input_method` / `ec_hitoolbox` | 非 macOS 可编，但是打印失败退出或空操作 |
| `scripts/setup.sh` | 仍装 WebKit/GTK/IBus，与现行 GPUI 宿主无关 |

**Linux IBus 残留的修正：** `platform/linux/ibus.rs` 已经发 `WindowEvent::UpdateWindowGeometry { RelativeToCaret }`，不是完全停留在 WebView 几何。但它仍依赖缺失的 `dbus`/`zbus`，也没有接到 `overlay.rs` 的 GPUI 路径。当作**草稿**，不是实现。

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
8. **`local_webview_data_dir` 这种名字不带进 Linux 安装布局。** 那是 WebView 时代的数据目录。

**仍开放、必须用后续 PR 关闭的问题：**

- gpui 0.2.2 能否在 Linux 上承载不激活 popup（C1 验收时决定是否升级；C2 只要求 `cargo check`/`clippy`，不要求浮层可见）。D3 用 `WindowKind::PopUp` + `_NET_WM_WINDOW_TYPE_NOTIFICATION` + `_NET_WM_STATE_ABOVE`，手测仍待 Linux 机器。
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

- 分支：`feat/cross-platform`（从 `main` @ `55b043ff` 切出）
- 下一步：Windows 手测 + named pipe 往返测试（F 代码已落树，CI 待 push）
- 本文件就是 M0 的文档交付物

进度勾选：

- [x] M0 文档（本文件，已经过评估修订）
- [x] PR-A1（`fig_util` Linux 常量；`fig_util` / `fig_os_shim` / `fig_proto` 在 `aarch64-unknown-linux-gnu` 上 `cargo check` 通过；macOS workspace clippy 绿）
- [x] PR-A2（`.github/workflows/ci.yml` 增加 `rust-linux`；不装 WebKit；首次 GitHub 原生 run 仍待推送后验证）
- [x] PR-A3（切断 `dbus` 编译图；`ec engine complete --buffer "git ch"` 本机成功含 `checkout`；Linux CI 扩到 `ec_cli`）
- [x] PR-B1（`fig_ipc` Linux 目标 `cargo check` 过；`figterm` scrollback 钉死为 1、tokio 2 worker 未改；socket 路径测试 + Linux CI 扩到 `fig_ipc`/`figterm`）
- [x] PR-B2（无桌面 `ec init` 与 macOS 同策略；zsh hook 在临时 HOME 可装可卸；IME 不进入 Linux `integrations install all`）
- [x] PR-C1（`ec_gpui` 的 AppKit 模块 `cfg(macos)`；非 Mac 走 `platform_stub`，`screens_quartz` 为空所以无 caret 则无法摆放；本机交叉编译卡在缺 linux gcc，Ubuntu 原生 gpui 仍待 CI）
- [x] PR-C2（`fig_desktop` 非 macOS 走 `platform/stub.rs`：`accessibility_is_enabled → None`、`get_cursor_position → None`、无 caret 则 park；`linux/` 与 `windows.rs` 不再编译；GNOME/IBus 安装路径切断；Linux CI 扩到 `ec_gpui`/`fig_desktop` clippy，系统依赖是 GTK/X11/Vulkan **不含 WebKit**。Ubuntu 原生 gpui 首次绿灯仍待推送后验证）
- [x] PR-D1（新建 `platform/linux_caret/`：X11 焦点跟踪 + zbus IBus；几何换算在 `platform/caret.rs` 单测钉死；无 caret 不摆放；旧 `platform/linux/` 仍不编译）
- [x] PR-D2（GPUI 0.2.2 无 layer-shell，浮层仍走 X11/XWayland。GNOME Wayland 终端 caret 走 AT-SPI `GetCharacterExtents(SCREEN)`，不用 Shell 扩展；窗口 `GetExtents` 只给 IBus relative 当原点，不当列表位置。无 a11y 总线或非终端 focus 则隐藏）
- [x] PR-D3（`ec_gpui/src/linux.rs`：按标题找 overlay，`unmap` park、`configure`+`map` 显示；启动时若有 `DISPLAY` 则清掉 `WAYLAND_DISPLAY` 让 GPUI 走 X11，`EC_GPUI_BACKEND=wayland` 可退出；无屏幕列表则 park，不用窗口矩形当 edges）
- [x] PR-E1 / PR-E2（`scripts/build-linux.sh` 前缀布局 + tar.gz；`scripts/install-linux.sh --prefix`；`.desktop` + hicolor 图标；不装 WebKit，不改 `build-app.sh`）
- [ ] 阶段 F（进行中：F1 named pipe + LocalListener；F2 `~user` 仅当前 USERPROFILE；F3 windows-latest CI 头less+desktop clippy；F4 GetGUIThreadInfo caret 无窗口矩形兜底；F5 GPUI HWND SetWindowPos；F6 zip 脚本。未在本机 Windows 手测）
