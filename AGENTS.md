# AGENTS.md

给在这个仓库里干活的编码 Agent 的说明。**先读这份，再动手改代码。**

用户文档在 `README.md`（面向最终用户）；这份是面向实现的，重点是「哪些地方看起来能改、其实一改就坏」。

---

## 1. 这是什么

跨平台局域网 KVM：在多台电脑之间共享鼠标、键盘、剪贴板（文本 + 文件）。

- **Primary（主机）**：跑 TCP hub，抓取真实输入，向邻居屏幕转发。
- **Secondary（从机）**：连上主机，接收并注入输入；自己不抓取输入。
- 语言：Rust 2021 / egui(eframe) 0.31 / rdev 0.5（vendored）/ arboard / core-graphics 0.25。
- 平台：macOS（主）+ Windows + Linux(X11)。**macOS 是参考实现**，其余平台部分路径是兜底。

一句话架构：**输入被 grab（不是 observe），协议只传相对 delta，跨屏在光标到达边缘之前预测。**
这两点是 v0.3.x「推倒重来」的核心结论，不要往回退化成 `rdev::listen` 观察者 + treadmill/绝对坐标——那套已经证明是错的（见 `.workbuddy/memory/2026-09-07.md`）。

---

## 2. 命令

```bash
# 构建（本地必用 release）
cargo build --release
./target/release/mouseshare          # 直接跑，不要 cargo run

# 测试（必须单线程）
cargo test -- --test-threads=1

# 交叉类型检查（CI 会构建这两个 target，改了 cfg(windows)/cfg(linux) 代码务必本地过一遍）
cargo check --target x86_64-pc-windows-msvc
cargo check --target x86_64-unknown-linux-gnu

# 格式
cargo fmt
```

**macOS debug 构建启动即 panic**（objc2/winit 的 ObjC 方法签名运行时校验，报 `expected return to have type code 'q', but found 'Q'`）。这不是你的 bug，但意味着 **debug 构建无法用来跑 GUI**。要本地看界面必须 `cargo build --release`。

**`cargo run` 同样不可用**（debug）。这是本仓库最容易浪费半小时的坑。

---

## 3. 模块地图

| 文件 | 职责 | 关键点 |
|---|---|---|
| `main.rs` | 接线：建窗口、起线程、Hello/EnterScreen 处理、布局探测 | 改任何线程接线先看这里的注释块 |
| `control.rs` | **控制面**：谁持有鼠标 + 跨屏状态机（`Local` / `Forwarding`） | 最大最敏感的文件（~2000 行）。路由单位是**机器** |
| `capture.rs` | 输入抓取。macOS = `CGEventTap`（真 grab）；其余 = `rdev::listen` 兜底 | 光标隐藏/脱离/回收都在这里 |
| `input.rs` | 输入注入 + 光标辅助（相对注入） | 跨平台 `warp_cursor` |
| `protocol.rs` | 线协议：`[u32 LE len][JSON(Message)]` | 帧的**顺序有语义**，见 §5 |
| `network.rs` | LAN 传输：hub + reader/writer 线程 | writer 是**按字节预算**的 |
| `layout.rs` | 屏幕布局模型（虚拟桌面坐标） | `Screen{ host, panel }`，机器 vs 显示器两级 |
| `discovery.rs` | UDP beacon 自动发现主机 | 端口 49153（与 TCP 49152 区分） |
| `transfer.rs` | 文件复制：manifest + 分块 + 重组 | 接收端路径净化在这里 |
| `clipboard.rs` | 剪贴板监控 + 回环抑制（`ClipState`） | 别破坏 generation 语义 |
| `clipfile.rs` | 原生剪贴板**文件**读写（macOS `public.file-url` / Win `CF_HDROP`） | Linux 是 no-op |
| `app.rs` | egui 窗口 + 可拖拽布局画布 | 只**组装**，不硬编码颜色/像素 |
| `ui.rs` | **设计系统**：`Theme` 调色板、字阶、`SP_*`/`R_*` 常量、原子组件 | 视觉的唯一真相源 |
| `i18n.rs` | 中英文案（`Lang` + `Tr`） | 所有面向用户的字符串都走这里 |
| `config.rs` | 持久化配置 | 见 §6 路径 |
| `diag.rs` | 文件诊断日志（决策点，非事件洪流） | `env_logger` 的 stderr 在 Finder 启动时看不见 |
| `single_instance.rs` | 单实例锁 | — |
| `tray.rs` | Windows 托盘图标 | 仅 Windows |
| `vendor/rdev` | rdev 0.5.3 本地补丁（跳过 TIS 字符名查询，修 macOS 13+ 崩溃） | `[patch.crates-io]`，别删 |

---

## 4. 构建/测试约定

- **测试是内联的**（`#[cfg(test)] mod`），没有 `tests/` 目录。分布在 `app.rs`、`capture.rs`、`control.rs`、`diag.rs`、`discovery.rs`、`input.rs`、`transfer.rs`。
- `control.rs::integration` 是**真 TCP** 端到端测试（primary hub + secondary over 127.0.0.1），跑通完整跨屏状态机。改控制面必须让它保持绿。
- **必须 `--test-threads=1`**：多个用例同时 handoff 会互相干扰。
- **测试 seam**：`cursor::hide/show/park`（capture.rs）、`apply_input`/`warp_cursor`（input.rs）、`diag::log`（diag.rs）在 `cfg(test)` 下都是 **no-op**。这样 headless 也不会碰 WindowServer / 真实输入 / 污染用户的 `mouseshare.log`。**加新的副作用 API 时，给它配一个 test no-op seam**，否则测试会变成破坏性操作。
- 跑集成测试时：客户端 `connect_client` 后**必须主动发 `Hello`**，hub 才会注册 peer（`connect_client` 自身不发）。
- **Agent 环境通常没有 GUI session**（`SECURITYSESSIONID` 为空），egui 窗口渲染不出来 —— 所以逻辑验证靠上面的真 TCP 集成测试，视觉验证靠截图技能（§8）。

---

## 5. 硬约束 / 地雷（改之前务必读完）

这些每一条都是真出过线上 bug 的，不是理论风险。

1. **event-tap 回调可达的整条调用链，禁止 `lock(ctx.layout)`。**
   能拿到 `Ctrl.layout_snap`（启动时快照）就用它。历史上 `return_control()` / `cycle_control()` 在这里漏锁，导致 tap 阻塞 → `TapDisabledByTimeout` → 双光标 + 持续卡顿的恶性循环。
   *加任何新函数，先问：它会不会从 tap callback 被调到？*

2. **`capture::disable_app_nap()` 必须在建窗口之前调用，且 token 要永久持有。**
   窗口不可见时 macOS 会把进程判为 idle，送 `TapDisabledByTimeout`；tap 禁用期间本地事件直通系统，重新 arm 后又继续 drop+forward —— **这就是「一鼠双光标」和「缩小后卡顿」的共同根因**。token 丢了 = activity 结束 = App Nap 回来。

3. **tap mask 里绝不能出现 `TapDisabledByTimeout`(0xFFFFFFFF) / `TapDisabledByUserInput`(0xFFFFFFFE)。**
   它们是**伪事件、不可 mask**，macOS 无论如何都会投递。放进去 `1u64 << event_type` 会在 debug 版直接 panic（capture 线程秒死），release 版是 UB。这两个是**删掉**，不是加条件过滤。

4. **合成/回收光标的事件必须打标 `kCGEventSourceUserData = capture::SYNTHETIC_MARK`。**
   否则 tap 会把自己的 warp 当成用户输入再转发一次。

5. **协议帧顺序有语义：`EnterScreen` 必须排在解释它的 `MouseMotion` 之前。**
   曾为了降低延迟把输入帧提到文件帧前面发，结果 3 个集成测试立刻挂掉，已回滚。**不要重排 `pump_writes` 的帧序。**

6. **只传相对 delta，永远不传绝对坐标。** 传绝对坐标要求两端屏幕几何/缩放完全同步，多屏 + 缩放下必然漂移。

7. **文件分块 = `FILE_CHUNK` 64 KiB，writer = `WRITE_BATCH_BYTES` 128 KiB/趟。**
   调大（曾经 256 KiB + 无上限单次 write）会饿死输入帧，表现为「跨屏时突然卡顿」。这是 0.9.0 修的卡顿根因，别再调回去。

8. **接收端路径净化不能省。** manifest 里的相对路径来自网络，是**不可信输入**；`safe_rel` 剥掉 `..`、绝对路径、`C:`，再配 `starts_with(root)` 兜底。接收侧的大小上限也要复查（防对端填满磁盘）。

9. **macOS fullsize content view 下必须 `.with_titlebar_shown(false)`。**
   少了它 macOS 会画一条**不透明**标题栏盖住内容顶部，把顶栏右上的控件裁掉（`titlebar_shown=false` 才映射到 winit 的 `titlebar_transparent`）。

10. **顶层 Panel 的 top margin 至少 30pt（macOS）。** 原生标题栏约 28pt 高，`top: 14` 会把控件压在标题栏底下——**而且缩略图截图看不出来**，必须原分辨率截关键区域。

11. **输入抓取是 grab，不是 observe。** 不要引入「事件照常进系统 + 事后 warp」的写法，会产生 echo 和跳变，并被迫重新引入 treadmill 补丁。

12. **`Screen.host` + `RemoteCtrl.panel` 的两级路由不能塌缩成按屏幕名路由。**
    一台多显示器机器在布局里是多块 panel；从哪块屏、哪条边返回必须靠 `panel` 决定，按 bbox 猜会在死区误判。

---

## 6. 运行期路径

| 内容 | 路径（macOS） |
|---|---|
| 配置 | `~/Library/Application Support/mouseshare/config.json` |
| 诊断日志 | 同目录下 `mouseshare.log` |
| 接收到的文件 | 系统临时目录 `.../mouseshare/clipboard/` |

排查线上问题时，**先数 `mouseshare.log`**（`CLIP-FILES-DETECTED` / `FILE-SEND` / `FILE-RECV` / `HAND-OFF` / `RETURN` / `ENTER`），不要凭现象猜。日志里没有的那一行，往往就是 bug 所在（例如某个早退分支只写了 `log::warn!` → stderr → Finder 启动时直接丢掉）。**给每个失败分支都补一条 `diag::log`**。

---

## 7. 代码约定

- **i18n**：任何面向用户的字符串都必须走 `crate::i18n`，中英**同时**给。只加一种语言算没写完。
- **UI**：`app.rs` 不硬编码颜色/像素值，一律取 `ui.rs` 的 `Theme` / `SP_*` / `R_*`。
- **文档**：行为变了就更新 `README.md`（用户向）。**本仓库没有也不需要 `AGENTS.md` 之外的第二份 agent 说明**——如果你看到项目记忆里提到「同步更新 AGENTS.md」，指的就是这份文件。
- **注释解释「为什么」**，尤其是反直觉的常数和 cfg 门控（现有代码注释密度很高，跟随这个风格）。跨平台 cfg 依赖：只在某平台用的 crate 必须写进 `[target.'cfg(...)'.dependencies]`，且 `use` 也要 `#[cfg(...)]`——曾经顶层无条件 `use core_foundation` 导致 Windows/Linux 打包 `E0433`。
- **不引入新依赖**除非确有必要：依赖直接影响三平台 CI 时长（例如 `display-info` 被门控在 macOS，否则 Windows 会拉进整个 `windows` 0.62 meta-crate，每次发版多 20 分钟）。

---

## 8. 发布流程

```bash
# 1. 确认干净起点
git status --short && git log --oneline -3

# 2. 改版本号（README 里的截图/描述若涉及版本也一并看）
#    Cargo.toml: version = "x.y.z"

# 3. 三件套全绿
cargo test -- --test-threads=1
cargo check --target x86_64-pc-windows-msvc
cargo fmt

# 4. 视觉验证（不要跳！）
#    用 macos-gui-screenshot-verify 技能：release 构建 → 按窗口 ID 截图 → 逐像素校验
#    教训：历史上多次「截图缩略图看着对、实际被裁/错位」。发布前一定要看图。

# 5. 清脚手架：确认没有临时开关/调试分支混进发布
grep -rn "TEMP-PROBE\|TEMP-DEV\|MS_DEV\|LANG-BTN" src/    # 必须为空

# 6. 提交 + 打 tag + 推 —— 两个都要推
git commit -m "vX.Y.Z: ..."
git tag -a vX.Y.Z -m "vX.Y.Z — ..."
git push origin main --tags          # 只 push main 会漏 tag，历史上真的漏过

# 7. 验证 CI：三平台全绿 + Release 5 个资产
#    mouseshare-linux.tar.gz / mouseshare-linux.deb /
#    mouseshare-windows.zip / MouseShare-Setup.exe / mouseshare-mac.dmg
```

- CI 触发条件：push 到 `main`、push `v*` tag、PR、手动。
- **`cargo fmt` 会顺手重排 4 个与本次改动无关的文件**（`build.rs`、`src/discovery.rs`、`src/single_instance.rs`、`src/ui.rs` —— 仓库整体 fmt 不干净是既有的）。**改完把无关文件 `git checkout --` 还原**，保持 diff 可评审；不要把它们混进功能 commit。
- 提交信息写**根因**而不是现象（看 `git log` 的风格：每条都说清「原来错在哪、为什么错」）。

---

## 9. 改完之后

- 做了实质工作（修 bug / 改架构 / 发版）→ 往 `.workbuddy/memory/YYYY-MM-DD.md` **追加**一段（append-only，别覆盖）：改了什么、真根因、怎么验证的。
- 跨会话有价值的项目级约定 → 写 `.workbuddy/memory/MEMORY.md`。
- 验证过的、可复用的操作流程 → 存成 skill 而不是只写记忆。
