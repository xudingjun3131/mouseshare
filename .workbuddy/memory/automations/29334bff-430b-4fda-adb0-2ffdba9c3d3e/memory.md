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
