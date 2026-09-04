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
    layout_tip: "提示：让屏幕边缘对齐，光标才能直接跨屏。",
    legend_primary: "主机",
    legend_me: "本机",
    legend_client: "客户端",
    err_title: "⚠ 启动异常：",
    err_hint: "窗口已正常打开。可在左侧修改配置并保存，然后重启本应用。",
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
    layout_tip: "Tip: align screen edges so the cursor can cross directly from one to the next.",
    legend_primary: "Primary",
    legend_me: "This machine",
    legend_client: "Client",
    err_title: "⚠ Startup error: ",
    err_hint: "The window is open. Edit the config on the left, save, then restart the app.",
};

/// Look up the string table for a language.
pub fn tr(lang: Lang) -> Tr {
    match lang {
        Lang::Zh => ZH,
        Lang::En => EN,
    }
}
