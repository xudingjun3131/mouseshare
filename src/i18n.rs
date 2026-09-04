//! Minimal i18n: Chinese / English UI strings.
//!
//! The language is stored in `Config.lang` ("zh" | "en", default "zh") and toggled from the
//! title bar. `Tr` is a plain Copy struct of static strings — cheap to pass around in egui
//! closures without lifetime headaches.

/// UI language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn from_code(s: &str) -> Lang {
        if s.eq_ignore_ascii_case("en") {
            Lang::En
        } else {
            Lang::Zh
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// Label shown on the toggle button — always the *other* language,
    /// so the button reads as an action ("switch to English / 切换到中文").
    pub fn toggle_label(self) -> &'static str {
        match self {
            Lang::Zh => "English",
            Lang::En => "中文",
        }
    }

    pub fn toggled(self) -> Lang {
        match self {
            Lang::Zh => Lang::En,
            Lang::En => Lang::Zh,
        }
    }

    /// Startup error: the primary could not bind its listen port.
    pub fn listen_fail(self, port: u16, err: impl std::fmt::Display) -> String {
        match self {
            Lang::Zh => format!(
                "无法监听端口 {}（{}）。端口很可能已被另一个正在运行的 MouseShare 占用——请检查 Dock 或活动监视器里是否已有 MouseShare，退出后重新启动。",
                port, err
            ),
            Lang::En => format!(
                "Cannot listen on port {} ({}). The port is most likely taken by another running MouseShare — check the Dock / Activity Monitor, quit it, then start again.",
                port, err
            ),
        }
    }

    /// Startup error: a secondary could not reach the primary.
    pub fn connect_fail(self, addr: &str, err: impl std::fmt::Display) -> String {
        match self {
            Lang::Zh => format!(
                "无法连接到主机 {}（{}）。请确认主机上的 MouseShare 已启动、地址正确、防火墙未拦截；修改地址后点「连接主机」即可重连。",
                addr, err
            ),
            Lang::En => format!(
                "Cannot connect to primary {} ({}). Make sure MouseShare is running on the primary, the address is correct, and no firewall is blocking it. Fix the address, then click \"Connect to host\".",
                addr, err
            ),
        }
    }
}

/// All user-facing UI strings.
#[derive(Clone, Copy)]
pub struct Tr {
    pub tagline: &'static str,
    pub section_basic: &'static str,
    pub machine_name: &'static str,
    pub role: &'static str,
    pub role_primary: &'static str,
    pub role_secondary: &'static str,
    pub server_addr: &'static str,
    pub listen_port: &'static str,
    pub detect_ip: &'static str,
    pub address: &'static str,
    pub copy_addr: &'static str,
    pub copied: &'static str,
    pub primary_name: &'static str,
    pub save: &'static str,
    pub saved_hint: &'static str,
    pub section_screens: &'static str,
    pub screens_hint: &'static str,
    pub add_screen: &'static str,
    pub dup: &'static str,
    pub del: &'static str,
    pub keep_one: &'static str,
    pub section_status: &'static str,
    pub peers: &'static str,
    pub local_name: &'static str,
    pub layout_title: &'static str,
    pub layout_hint: &'static str,
    pub layout_tip: &'static str,
    pub legend_primary: &'static str,
    pub legend_me: &'static str,
    pub legend_client: &'static str,
    pub err_title: &'static str,
    pub err_hint: &'static str,
    pub connect_host: &'static str,
    pub retry_connect: &'static str,
    pub reconnect_host: &'static str,
    pub connected: &'static str,
    pub conn_status: &'static str,
    pub conn_primary: &'static str,
    pub conn_connected: &'static str,
    pub conn_idle: &'static str,
    pub ctrl_status: &'static str,
    pub ctrl_local: &'static str,
    pub ctrl_remote: &'static str,
    pub ctrl_pushing: &'static str,
    pub hotkey_hint: &'static str,
    pub background_hint: &'static str,
    pub exit_app: &'static str,
}

pub const ZH: Tr = Tr {
    tagline: "通过局域网共享鼠标、键盘与剪贴板",
    section_basic: "基本设置",
    machine_name: "本机名称（需唯一）:",
    role: "角色",
    role_primary: "主机（服务端，接真实鼠标键盘）",
    role_secondary: "副机（接收输入）",
    server_addr: "主机地址（ip:端口）:",
    listen_port: "监听端口:",
    detect_ip: "探测本机局域网 IP",
    address: "连接地址:",
    copy_addr: "复制连接地址",
    copied: "✓ 已复制连接地址到剪贴板",
    primary_name: "主机名称（须与主机上配置的本机名称一致）:",
    save: "保存配置",
    saved_hint: "✓ 已保存。角色 / 网络变更需重启应用后生效。",
    section_screens: "屏幕与客户端",
    screens_hint: "客户端数量无上限：连上的机器会自动加入布局。也可以在这里复制或删除屏幕。",
    add_screen: "＋ 添加屏幕",
    dup: "复制",
    del: "删除",
    keep_one: "至少保留一块屏幕",
    section_status: "运行状态",
    peers: "已连接设备: ",
    local_name: "本机名称: ",
    layout_title: "屏幕布局 — 拖动屏幕调整位置",
    layout_hint: "按桌面上的实际摆放排布各块屏幕。主机（高亮）是真实光标所在，光标越过边缘即把控制权交给相邻机器。",
    layout_tip: "提示：拖动副机色块贴住主机屏幕边缘（自动吸附对齐），再把鼠标持续推向该边缘即可跨屏。",
    legend_primary: "主机",
    legend_me: "本机",
    legend_client: "客户端",
    err_title: "⚠ 启动异常：",
    err_hint: "窗口已正常打开。可在左侧修改配置并保存，然后重启本应用。",
    connect_host: "连接主机",
    retry_connect: "重新连接主机",
    reconnect_host: "重新连接主机",
    connected: "✓ 已连接到主机",
    conn_status: "连接状态: ",
    conn_primary: "主机（正在服务）",
    conn_connected: "已连接主机",
    conn_idle: "未连接",
    ctrl_status: "鼠标控制权",
    ctrl_local: "本机（推向副机一侧的屏幕边缘即可跨屏）",
    ctrl_remote: "当前在 {}（向主机方向推回边缘即返回）",
    ctrl_pushing: "贴边推进 {n}/{total}，继续向外推…",
    hotkey_hint: "切换鼠标控制权快捷键：Ctrl+Alt+空格 或 ScrollLock，在主机与各副机之间轮换（Mac、Windows 两端均可按）。",
    background_hint: "关闭窗口后 MouseShare 会最小化到后台继续共享；要彻底退出请点下方按钮。",
    exit_app: "退出程序",
};

pub const EN: Tr = Tr {
    tagline: "Share mouse, keyboard & clipboard over LAN",
    section_basic: "Basic",
    machine_name: "This machine's name (unique):",
    role: "Role",
    role_primary: "Primary (server, has the real mouse/keyboard)",
    role_secondary: "Secondary (receives input)",
    server_addr: "Primary address (host:port):",
    listen_port: "Listen port:",
    detect_ip: "Detect my LAN IP",
    address: "Address:",
    copy_addr: "Copy address",
    copied: "✓ Address copied to clipboard",
    primary_name: "Primary machine name (must match that machine's name):",
    save: "Save config",
    saved_hint: "✓ Saved. Restart the app for role/network changes to take effect.",
    section_screens: "Screens & Clients",
    screens_hint: "Unlimited clients: machines appear in the layout automatically once connected. You can also duplicate or remove screens here.",
    add_screen: "＋ Add screen",
    dup: "Duplicate",
    del: "Remove",
    keep_one: "At least one screen is required",
    section_status: "Status",
    peers: "Connected peers: ",
    local_name: "Local name: ",
    layout_title: "Screen layout — drag a screen to reposition it",
    layout_hint: "Place screens the way they sit on your desk. The primary (highlighted) is where your real cursor lives; cross an edge to hand control to a neighbour.",
    layout_tip: "Tip: drag a secondary flush against the primary's screen edge (it magnet-snaps), then keep pushing the mouse into that edge to cross over.",
    legend_primary: "Primary",
    legend_me: "This machine",
    legend_client: "Client",
    err_title: "⚠ Startup error: ",
    err_hint: "The window is open. Fix the address on the left, then click \"Connect to host\" to reconnect, or restart the app.",
    connect_host: "Connect to host",
    retry_connect: "Retry connect",
    reconnect_host: "Reconnect to host",
    connected: "✓ Connected to host",
    conn_status: "Status: ",
    conn_primary: "Primary (serving)",
    conn_connected: "Connected to host",
    conn_idle: "Not connected",
    ctrl_status: "Mouse control",
    ctrl_local: "This machine (push into a secondary's edge to cross over)",
    ctrl_remote: "On {} (push back toward the primary's edge to return)",
    ctrl_pushing: "Edge push {n}/{total} — keep pushing…",
    hotkey_hint: "Switch-hotkey: Ctrl+Alt+Space or ScrollLock rotates the mouse between the primary and each secondary (works on both sides).",
    background_hint: "Closing this window minimises MouseShare to the background and sharing keeps running. Use the button below to quit for real.",
    exit_app: "Quit MouseShare",
};

/// Look up the string table for a language.
pub fn tr(lang: Lang) -> Tr {
    match lang {
        Lang::Zh => ZH,
        Lang::En => EN,
    }
}
