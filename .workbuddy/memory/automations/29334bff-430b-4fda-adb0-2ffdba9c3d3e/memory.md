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
