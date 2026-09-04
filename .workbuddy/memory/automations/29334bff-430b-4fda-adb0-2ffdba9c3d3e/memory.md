# Auto Commit / Tag / Release — 执行记录

## 运行环境要点
- 仓库：/Users/xudinhgjun/projects/ai/mouseshare（GitHub: xudingjun3131/mouseshare，默认分支 main）
- cargo 需 `export PATH="$HOME/.cargo/bin:$PATH"`
- CI 状态查询用 `curl -s -H "Accept: application/vnd.github+json" https://api.github.com/repos/xudingjun3131/mouseshare/commits/<SHA>/check-runs` 比 WebFetch 更准确（拿到原始 JSON，不丢字段）
- 三个 job 名：ubuntu-latest / windows-latest / macos-latest
- 调试日志贴 GitHub Issue #1

## 执行历史

### 2026-09-04 08:35（首次执行）
- HEAD 702f6a8，与 origin/main 同步，最新 tag v0.3.3
- 唯一改动：.workbuddy/memory/2026-09-03.md（记忆文件）→ 走「仅 .workbuddy 改动」分支
- 提交 13767a9 `docs: update memory` 并 push，未打 tag、未发版
- 顺带核查 v0.3.3 状态：三平台 check-run 全 completed+success，Release 5 个资产齐全
  （deb / linux.tar.gz / mac.dmg / Setup.exe / windows.zip）
- 结论：无需修复动作

### 2026-09-04 09:20（第二次执行）
- HEAD 32355c1，与 origin/main 同步（0 0），有产品代码改动：src/app.rs、src/main.rs
- 提交 6de8175 `feat: auto-commit dev changes` 并 push，打 tag v0.3.6
- 执行中被用户打断反馈 UI 遮挡：屏幕布局区蓝色屏幕块压到标题/提示
- 修复 src/app.rs：draw_layout 改用 available_rect_before_wrap 并以画布矩形原点绘制
- 提交 e5a06bb `fix: prevent layout tiles from overlapping header` 并 push，打 tag v0.3.7
- 等待后 CI 三平台（ubuntu/windows/macos）全部 completed+success
- Release v0.3.7 资产齐全（deb / linux.tar.gz / mac.dmg / Setup.exe / windows.zip）
- 结论：发版成功，无需进一步修复

### 2026-09-04 09:45（第三次执行，崩溃修复）
- 用户报 v0.3.7 启动后崩溃（macOS 26），崩溃栈：rdev::macos::listen::raw_callback → HIToolbox TSMGetInputSourceProperty → dispatch_assert_queue_fail（EXC_BREAKPOINT）
- 根因：rdev 0.5.3 在事件 tap 线程调 TIS 键盘布局 API，macOS 13+ 要求主线程 → 断言杀进程（rdev issue #146）
- mouseshare 不依赖事件的 name 字符字段，所以 vendored rdev 0.5.3 到 vendor/rdev 并让 string_from_code 直接返回 None，Cargo.toml 加 [patch.crates-io] rdev = { path = "vendor/rdev" }
- 顺带修 IP：primary 模式启动时 local_ip_address::local_ip() 自动写入 server_addr，替代写死的 192.168.1.100
- 提交 ad89da6，打 tag v0.3.8，CI 三平台全绿，Release v0.3.8 资产齐全
- 结论：崩溃修复 + IP 自动探测完成，发版成功

### 2026-09-04 10:05（第四次执行，字体统一）
- 用户反馈英文界面与中文界面「样式不一样」
- 根因：i18n 整段切换（Zh/En 两套静态文案）；setup_fonts 把 cjk(Noto Sans SC) 追加到字体族末尾做 fallback → 中文模式回退 Noto、英文模式走 egui 默认 sans，整窗字体观感随语言切换变化
- 修复 src/app.rs setup_fonts：抽 prefer_cjk 闭包，把 "cjk" 用 insert(0,...) 提到 Proportional/Monospace 族队首，中英文共用 Noto Sans SC（含拉丁字形），默认字体退居兜底
- 提交 83e5075，打 tag v0.3.9，CI 三平台全绿，Release v0.3.9 资产齐全
- 结论：中英文样式统一，发版成功

### 2026-09-04 10:50（第五次执行，英文布局修复）
- 用户截图反馈英文版右侧蓝色屏幕块过大贴边、顶部 hint 和底部 tip 被截断，中文版正常
- 根因：CentralPanel header 依赖 `ui.available_rect_before_wrap()` 推导 canvas 区域；英文 hint 在 `ui.horizontal` 中过长溢出，cursor 状态异常，canvas 区域被错误放大
- 修复 src/app.rs：header 包进 `ui.vertical` 取实际高度，用 `ui.max_rect()` 减去 header 得到精确 canvas_rect 传给 draw_layout；hint/tip 改用 `Label::wrap()` 强制换行避免溢出
- 提交 8bb9fb8，打 tag v0.3.10，CI 三平台全绿，Release v0.3.10 资产齐全
- 结论：英文版布局修复，发版成功

### 2026-09-04 11:10（第六次执行，副机连接 + 布局同步）
- 用户报 Windows 副机连不上 Mac 主机（地址端口对）、且副机界面无连接入口、无主机屏幕
- 修复：新增 `Message::Layout` 协议让主机把布局推给副机（副机即可看到 Mac 屏）；副机新增 `reconnect()` + 「连接主机/重试/重连」按钮（运行中点保存不再需要重启）；主机每 2s 节流 broadcast_layout；副机断线 net 置 Idle 并显示状态
- 提交 37a7279，打 tag v0.3.11，CI 三平台全绿（6 check run 全 success），Release v0.3.11 资产齐全
- 结论：副机连接入口 + 主机屏幕显示修复，发版成功
- 排障：本机 curl 调 GitHub API 匿名限流，改用 WebFetch 查 check-runs/release

### 2026-09-04 11:25（第七次执行，Windows 图标）
- 用户报 Windows 版的 logo 没带上
- 根因：Windows 构建完全没接图标（无 .ico / 无 .rc / 无 winresource / installer.iss 无 SetupIconFile）；macOS 用 AppIcon.icns、Linux 用运行时 with_icon 都正常
- 修复：Pillow 由 mouse-logo.png 生成 resources/AppIcon.ico（10 档 16–256）；新增 build.rs 用 winresource 把 .ico 编进 .exe（非 Windows 为空 main）；Cargo.toml 加 winresource build-dep；installer.iss 加 SetupIconFile
- 提交 d38053b，打 tag v0.3.12；windows/macos completed+success，Release v0.3.12 资产齐全（含 Setup.exe/windows.zip）

### 2026-09-04 11:26（第七次执行，仅记忆更新）
- HEAD 与 origin/main 同步（0 0），工作区改动仅 2 个文件且都在 .workbuddy/ 下
  （memory/2026-09-04.md 与 automation memory.md 本身）
- 走「仅 .workbuddy 改动」分支：提交 `docs: update memory` 并 push，不打 tag、不发版
- 无产品代码改动，CI 无需动作
- 结论：仅记忆/文档同步，无需发版

### 2026-09-04 12:10（第八次执行，多屏 + 跨屏修复）
- 用户报：Mac(主机) 鼠标移不到 Win(副机)；Mac 双显示器只识别一块
- 根因：① `ensure_screen` 留 40px 死区 + `clamp` 把虚拟光标 snap 回 → 跨屏结构性不可能；② 主机布局写死单屏，无运行时显示器枚举
- 修复：Screen 加 is_local 标志（本机多屏均 true、副机 false）；ensure_screen 改为紧贴无死区；detect_primary_layout 用 display-info 枚举 Mac 真实显示器（CGDisplayBounds 坐标与 rdev 一致）；handle_capture 重写跨屏转发 + treadmill（基于本地包围盒、仅外侧有副机屏时 warp）
- 提交 9bb680a 打 tag v0.3.13；随后 b1131bc 把 display-info 限定 macOS-only（避免 Win/Linux 编 windows 0.62 巨无霸）
- v0.3.13 的 tag CI 在 Windows 卡 56+ 分钟（display-info 未限定平台时 Win 编 windows 0.62 + LTO 疑似卡死）；改为从优化后的 b1131bc 打 tag **v0.3.14**
- Release v0.3.14 已发布（draft:false），5 资产齐全；win/mac/linux 三平台 CI 全绿（Win 仅 3 分钟）
- 结论：多屏识别 + 跨屏修复发版成功，让用户用 v0.3.14

### 2026-09-04 12:28（第八次执行，空跑）
- `git fetch` 后工作区 `git status --porcelain` 为空，且 `HEAD...origin/main` 为 "0 0"
- 判定无事可做，直接结束；未提交、未打 tag、未触发 CI
- 结论：仓库已与远端同步，无待发布改动

