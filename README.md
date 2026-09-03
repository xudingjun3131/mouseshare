# MouseShare

在多台电脑之间共享**鼠标、键盘、剪贴板**，基于局域网（LAN），开源、跨平台（Windows / Linux / macOS）。

每台机器运行同一个程序。其中一台作为 **Primary（主机，连着真实的鼠标键盘）**，其余作为 **Secondary（从机，接收输入）**。界面里可以像操作系统显示器设置那样**拖动每台电脑的屏幕位置**——把屏幕边缘对齐，鼠标就能平滑地“跨”到另一台电脑上。

```
   [ Primary 1920x1080 ] [ Secondary 1920x1080 ]
   ← 鼠标移到右边缘 → 控制权交给 Secondary，光标出现在它屏幕左侧
```

## 特性

- 🖱️ 跨平台鼠标 / 键盘共享（基于 [rdev](https://github.com/Narsil/rdev)）
- 📋 剪贴板双向同步（基于 [arboard](https://github.com/1Password/arboard)，带回环抑制）
- 🖥️ 可拖拽的屏幕布局（基于 [egui](https://github.com/emilk/egui)），所见即所得
- 🔢 **客户端数量无上限**：从机连上 Primary 后，自动登记为布局里的一块屏幕，无需手动添加
- 📑 布局里可**一键复制 / 删除**任意屏幕，快速搭好多机布局
- 📎 Primary 界面可**一键复制连接地址**（`host:port`），直接粘给新机器填
- 🌐 局域网 TCP 传输，长度前缀 + JSON 协议，轻量可靠
- 📦 GitHub Actions 自动为 Win / Linux / Mac 三个平台产出安装包

## 快速开始

### 1. 在一台机器上当 Primary（主机）

1. 启动 `mouseshare`，角色选 **Primary (server)**。
2. 点击 **Detect my LAN IP** 获取本机局域网地址（如 `192.168.1.20:49152`）。
3. 在布局里把这台机器的屏幕放好（默认已在原点）。

### 2. 在其它机器上当 Secondary（从机）

1. 启动 `mouseshare`，角色选 **Secondary (client)**。
2. 填入 Primary 的地址（`192.168.1.20:49152`）。可以在 Primary 界面点 **复制连接地址** 一键复制，再粘过来。
3. 连上后，这块屏幕会**自动出现**在 Primary 的布局里（客户端数量无上限）。把它拖到 Primary 屏幕旁边即可，无需手动添加。
   - 也可以用界面里的 **复制** / **删除** 按钮管理屏幕：复制会克隆当前屏幕（名称自动加 `-copyN`、并右移避免重叠），方便快速搭好多机布局。
4. **重要**：每台机器的屏幕 `name` 必须和那台机器配置里的 `name` 一致（默认是主机名）。所有机器共享同一份布局文件，建议统一编辑好后分发。

### 3. 使用

把真实鼠标移到 Primary 屏幕边缘、对准邻居屏幕的方向，继续推——光标会“跨”到邻居屏幕上，键盘和剪贴板也随之切换过去。往回推则切回 Primary。

## 权限要求

- **macOS**：需要在 *系统设置 → 隐私与安全性 → 辅助功能* 中，给运行 MouseShare 的终端 / App 授权（输入捕获与注入都需要）。
- **Linux**：需要在 X11 会话下运行（Wayland 下 rdev 的全局捕获不可用）；部分发行版需要 `xdotool`/输入权限。
- **Windows**：首次运行可能被杀软拦截，允许即可；以普通用户权限运行足够。

## 构建

```bash
cargo build --release
# 产物: target/release/mouseshare  (Windows 为 .exe)
```

本地打包：

- **Windows**：`iscc installer.iss` 生成 `MouseShare-Setup.exe`
- **macOS**：`mkdir -p MouseShare.app/Contents/MacOS && cp target/release/mouseshare MouseShare.app/Contents/MacOS/ && hdiutil create -volname MouseShare -srcfolder MouseShare.app -ov -format UDZO mouseshare-mac.dmg`
- **Linux**：见 CI 流程（`tar.gz` 与 `cargo deb` 生成的 `.deb`）

## CI 自动出包

推送代码到 `main` 或打 `v*` 标签即可触发 `.github/workflows/build.yml`：

- `ubuntu-latest` → `mouseshare-linux.tar.gz` + `mouseshare-linux.deb`
- `windows-latest` → `mouseshare-windows.zip` + `MouseShare-Setup.exe`
- `macos-latest` → `mouseshare-mac.dmg`

打 tag（如 `v1.0.0`）时，这些安装包会自动作为 GitHub Release 资产上传。

## 已知限制 / 后续可优化

- 跨平台的物理按键映射（如 macOS ⌘ 与 Windows Ctrl）目前按物理键位透传，未做语义翻译。
- 真正的“无边界光标”通过“跑步机”式边界回卷实现，体验接近 Synergy/Barrier，但在极端布局下可能有轻微跳变。
- 目前为单 Primary 拓扑；未来可加入点对点 / 多 Primary。
- 剪贴板仅同步纯文本（可扩展为图片 / 文件）。

## 许可证

MIT OR Apache-2.0
