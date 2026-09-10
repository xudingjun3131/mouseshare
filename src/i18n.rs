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

/// The language background threads should use for notifications. The GUI thread has the real
/// `Lang`; workers (file transfer) only need "whichever the user picked", so we mirror it here
/// once at startup.
static WORKER_LANG: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_lang(l: Lang) {
    WORKER_LANG.store(l as u8, std::sync::atomic::Ordering::Relaxed);
}

pub fn current_lang() -> Lang {
    if WORKER_LANG.load(std::sync::atomic::Ordering::Relaxed) == 1 {
        Lang::En
    } else {
        Lang::Zh
    }
}

/// A file copy exceeded the safety cap and was skipped.
pub fn tr_file_too_big() -> String {
    match current_lang() {
        Lang::Zh => "复制的文件超过 512 MB 上限，已跳过。".to_string(),
        Lang::En => "The copied files exceed the 512 MB limit and were skipped.".to_string(),
    }
}

/// A file copy finished arriving from another machine.
pub fn tr_file_received(n: usize) -> String {
    match current_lang() {
        Lang::Zh => format!("已接收 {} 个文件，可直接粘贴。", n),
        Lang::En => format!("Received {} file(s) — ready to paste.", n),
    }
}

/// A file copy finished being sent to the other machine(s).
pub fn tr_file_sent(n: usize) -> String {
    match current_lang() {
        Lang::Zh => format!("已发送 {} 个文件到其他设备。", n),
        Lang::En => format!("Sent {} file(s) to the other machine(s).", n),
    }
}

/// How an activity-feed entry should be coloured.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Info,
    Warn,
    Error,
}

/// Translate one diagnostic-log line into a readable activity-feed entry, or `None` to leave it
/// out of the feed.
///
/// The diagnostic log is written for debugging: a machine name, a port number, a bounding box,
/// a full screen list, all on one line. Rendering it verbatim — which is what the previous
/// design did — produced a feed of machine noise where the one line that mattered was six
/// identical startup banners deep. Anything a user would not act on is dropped here.
///
/// The trade-off: a newly added diagnostic event stays invisible until it is listed below.
/// Unrecognised lines that announce a failure are the exception — those are always surfaced,
/// because a silent failure in a tool whose whole job is invisible background work is the worst
/// possible outcome.
pub fn tr_event(lang: Lang, line: &str) -> Option<(String, Severity)> {
    let zh = lang == Lang::Zh;
    let lower = line.to_ascii_lowercase();
    let has = |n: &str| lower.contains(n);
    let t = |z: &str, e: &str| -> String {
        if zh { z.to_string() } else { e.to_string() }
    };

    if has("capture failed") {
        return Some((
            t(
                "输入捕获失败：缺少「辅助功能 / 输入监控」权限",
                "Input capture failed — Accessibility / Input Monitoring permission missing",
            ),
            Severity::Error,
        ));
    }
    if has("app nap disabled") {
        return Some((
            t("已关闭系统节能休眠，避免后台卡顿", "Disabled system power nap to prevent stalls"),
            Severity::Ok,
        ));
    }
    if has("capture thread started") {
        return Some((
            t("输入捕获已启动", "Input capture started"),
            Severity::Ok,
        ));
    }
    if has("startup mode=") {
        let role = if zh { "主机" } else { "primary" };
        return Some((
            t(&format!("已作为{role}启动"), "Started as primary"),
            Severity::Info,
        ));
    }
    if has("file-recv-first-chunk") || has("file-recv token=") {
        return Some((
            t("正在接收文件…", "Receiving files…"),
            Severity::Info,
        ));
    }
    if has("file-send aborted") {
        return Some((
            t("文件发送已中止", "File send aborted"),
            Severity::Warn,
        ));
    }
    if has("file-apply-failed") {
        return Some((
            t("写入系统剪贴板失败", "Could not write to the system clipboard"),
            Severity::Error,
        ));
    }
    if has("click inside ui window") {
        return Some((
            t(
                "点击了 MouseShare 窗口，鼠标控制权已收回",
                "Clicked the MouseShare window — mouse control returned",
            ),
            Severity::Info,
        ));
    }
    if has("tap disabled") {
        return Some((
            t(
                "系统暂停了输入捕获，已自动恢复",
                "macOS paused input capture — recovered automatically",
            ),
            Severity::Warn,
        ));
    }
    if has("leave") && has("->") {
        return Some((
            t("鼠标已跨到另一台设备", "Mouse crossed to the other machine"),
            Severity::Info,
        ));
    }
    if has("return <-") {
        return Some((
            t("鼠标已回到本机", "Mouse returned to this machine"),
            Severity::Info,
        ));
    }
    if has("hand-off") || has("enter screen") {
        return Some((
            t("鼠标已跨到另一台设备", "Mouse crossed to the other machine"),
            Severity::Info,
        ));
    }

    // Unknown. Surface it only if it reports a problem.
    let severe = ["error", "fail", "panic", "refused", "denied"]
        .iter()
        .any(|k| has(k));
    if severe {
        let trimmed: String = line.chars().take(140).collect();
        return Some((trimmed, Severity::Error));
    }
    None
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
    pub machine_name_hint: &'static str,
    pub role: &'static str,
    pub role_primary: &'static str,
    pub role_secondary: &'static str,
    pub role_primary_short: &'static str,
    pub role_secondary_short: &'static str,
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
    pub section_discovered: &'static str,
    pub discovered_empty: &'static str,
    pub discovered_connect: &'static str,
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
    /// Short form for the stat tile — the long sentence goes in the tile's footnote, because a
    /// stat value has one line and would otherwise be truncated mid-sentence.
    pub ctrl_local_short: &'static str,
    pub ctrl_remote: &'static str,
    pub ctrl_pushing: &'static str,
    pub hotkey_hint: &'static str,
    pub diag_hint: &'static str,
    pub background_hint: &'static str,
    pub exit_app: &'static str,
    pub tray_show: &'static str,
    // ---- Sidebar navigation ----
    pub nav_group_config: &'static str,
    pub nav_group_network: &'static str,
    pub nav_connection: &'static str,
    pub nav_layout: &'static str,
    pub nav_status: &'static str,
    pub nav_discovered: &'static str,
    // ---- Page headings ----
    pub page_connection: &'static str,
    pub page_connection_sub: &'static str,
    pub page_layout: &'static str,
    pub page_layout_sub: &'static str,
    pub page_status: &'static str,
    pub page_status_sub: &'static str,
    pub page_discovered: &'static str,
    pub page_discovered_sub: &'static str,
    // ---- Card headings ----
    pub card_role: &'static str,
    pub card_role_sub: &'static str,
    pub card_primary: &'static str,
    pub card_primary_sub: &'static str,
    pub card_secondary: &'static str,
    pub card_secondary_sub: &'static str,
    pub card_screens: &'static str,
    pub card_screens_sub: &'static str,
    pub card_canvas: &'static str,
    pub card_canvas_sub: &'static str,
    pub card_clients: &'static str,
    pub card_clients_sub: &'static str,
    pub card_stats: &'static str,
    pub card_activity: &'static str,
    pub card_activity_sub: &'static str,
    pub card_network: &'static str,
    pub card_network_sub: &'static str,
    pub card_discovered: &'static str,
    pub card_discovered_sub: &'static str,
    // ---- Role mode cards ----
    pub role_primary_card: &'static str,
    pub role_primary_desc: &'static str,
    pub role_secondary_card: &'static str,
    pub role_secondary_desc: &'static str,
    // ---- Form / misc ----
    pub local_ip: &'static str,
    pub port_hint: &'static str,
    // ---- Cross-screen pointer speed ----
    pub card_speed: &'static str,
    pub card_speed_sub: &'static str,
    pub speed_multiplier: &'static str,
    pub speed_hint: &'static str,
    pub speed_effective: &'static str,
    pub speed_no_peer: &'static str,
    pub copy: &'static str,
    pub screens_empty: &'static str,
    pub online: &'static str,
    pub offline: &'static str,
    // ---- Status stats ----
    pub stat_peers: &'static str,
    pub stat_peers_foot: &'static str,
    pub stat_conn: &'static str,
    pub stat_ctrl: &'static str,
    pub activity_empty: &'static str,
    // ---- Permission guidance dialog ----
    pub perm_title: &'static str,
    pub perm_body: &'static str,
    pub perm_open_input: &'static str,
    pub perm_open_accessibility: &'static str,
    pub perm_recheck: &'static str,
    pub perm_dismiss: &'static str,
}

pub const ZH: Tr = Tr {
    tagline: "通过局域网共享鼠标、键盘与剪贴板",
    section_basic: "基本设置",
    machine_name: "本机名称",
    machine_name_hint: "需唯一，用于在局域网中标识这台机器",
    role: "角色",
    role_primary: "主机（服务端，接真实鼠标键盘）",
    role_secondary: "副机（接收输入）",
    role_primary_short: "主机",
    role_secondary_short: "副机",
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
    section_discovered: "发现的设备（局域网）",
    discovered_empty: "正在搜索局域网中的主机…",
    discovered_connect: "连接",
    peers: "已连接设备: ",
    local_name: "本机名称",
    layout_title: "屏幕布局 — 拖动屏幕调整位置",
    layout_hint: "按桌面上的实际摆放排布各块屏幕。主机（高亮）是真实光标所在，光标越过边缘即把控制权交给相邻机器。",
    layout_tip: "提示：副机色块拖到主机屏幕边上（允许小误差）即可跨屏；把鼠标贴住该边缘保持不动约半秒、或向边缘一推即跨。橙色圆点 = 实时鼠标位置。",
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
    ctrl_local: "本机（推向副机一侧的屏幕边缘即跨屏）",
    ctrl_local_short: "本机",
    ctrl_remote: "当前在 {}（本机光标已隐藏；向主机方向推回边缘即返回）",
    ctrl_pushing: "贴边推进 {n}/{total}，继续向外推…",
    hotkey_hint: "切换鼠标控制权快捷键：Ctrl+Alt+空格 或 ScrollLock，在主机与各副机之间轮换（Mac、Windows 两端均可按）。",
    diag_hint: "跨屏诊断日志（如跨屏异常，请把此文件内容发给开发者）：",
    background_hint: "关闭窗口后 MouseShare 会最小化到后台继续共享；要彻底退出请点下方按钮。",
    exit_app: "退出程序",
    tray_show: "显示主窗口",
    // ---- 侧栏导航 ----
    nav_group_config: "配置",
    nav_group_network: "网络",
    nav_connection: "连接",
    nav_layout: "屏幕布局",
    nav_status: "状态",
    nav_discovered: "发现的设备",
    // ---- 页面标题 ----
    page_connection: "连接",
    page_connection_sub: "选择本机角色，并告诉副机如何连接",
    page_layout: "屏幕布局",
    page_layout_sub: "把屏幕块摆成和你桌面一样的相对位置 · 橙色点是真实光标",
    page_status: "运行状态",
    page_status_sub: "连接统计、网络状态与最近的控制决策",
    page_discovered: "发现的设备",
    page_discovered_sub: "局域网中广播了信标的主机，点一下即可连接",
    // ---- 卡片标题 ----
    card_role: "我的角色",
    card_role_sub: "决定谁接真实的鼠标和键盘",
    card_primary: "主机配置",
    card_primary_sub: "告诉副机如何连到这台机器",
    card_secondary: "副机配置",
    card_secondary_sub: "填入主机的地址和端口",
    card_screens: "本地屏幕",
    card_screens_sub: "它们会自动加入主屏布局",
    card_canvas: "虚拟桌面",
    card_canvas_sub: "拖动来调整位置 · 橙色圆点是真实鼠标位置",
    card_clients: "连接的客户端",
    card_clients_sub: "拖到上面画布加入布局",
    card_stats: "本次会话",
    card_activity: "最近事件",
    card_activity_sub: "来自跨屏诊断日志的真实记录",
    card_network: "网络",
    card_network_sub: "尚未连接到任何主机",
    card_discovered: "局域网中的主机",
    card_discovered_sub: "通过 UDP 信标自动发现",
    // ---- 角色卡片 ----
    role_primary_card: "主机（Primary）",
    role_primary_desc: "这台机器接真实的鼠标和键盘，并将它们共享给局域网中的所有副机。",
    role_secondary_card: "副机（Secondary）",
    role_secondary_desc: "接收来自主机的输入，或直接把鼠标划到屏幕边缘进入主机。",
    // ---- 表单 / 其它 ----
    local_ip: "本地 IP",
    port_hint: "TCP 端口，副机用它连接本机",
    card_speed: "跨屏鼠标速度",
    card_speed_sub: "程序按两台机器的缩放比例自动换算，这里只做微调",
    speed_multiplier: "手动微调",
    speed_hint: "1.00 = 完全自动；觉得快就调小，觉得慢就调大",
    speed_effective: "实际换算系数",
    speed_no_peer: "等待副机连接…",
    copy: "复制",
    screens_empty: "还没有屏幕。",
    online: "在线",
    offline: "离线",
    // ---- 状态统计 ----
    stat_peers: "已连接设备",
    stat_peers_foot: "台副机",
    stat_conn: "连接状态",
    stat_ctrl: "鼠标控制权",
    activity_empty: "还没有记录。开始跨屏后，这里的决策日志会实时更新。",
    // ---- 权限引导弹窗 ----
    perm_title: "需要输入监控与辅助功能权限",
    perm_body: "MouseShare 需要两项系统权限才能抓取鼠标/键盘并跨屏：\n① 输入监控 —— 读取键盘与鼠标事件\n② 辅助功能 —— 注入光标位置\n若 macOS 未自动弹出授权提示、或此前被拒绝了，请点下方按钮到系统设置，把 MouseShare 的开关打开，然后回到这里点「重新检测」。",
    perm_open_input: "打开系统设置 · 输入监控",
    perm_open_accessibility: "打开系统设置 · 辅助功能",
    perm_recheck: "我已授权，重新检测",
    perm_dismiss: "稍后再说",
};

pub const EN: Tr = Tr {
    tagline: "Share mouse, keyboard & clipboard over LAN",
    section_basic: "Basic",
    machine_name: "Machine name",
    machine_name_hint: "Must be unique — it identifies this machine on the LAN",
    role: "Role",
    role_primary: "Primary (server, has the real mouse/keyboard)",
    role_secondary: "Secondary (receives input)",
    role_primary_short: "Primary",
    role_secondary_short: "Secondary",
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
    section_discovered: "Discovered on LAN",
    discovered_empty: "Searching for a primary on the LAN…",
    discovered_connect: "Connect",
    peers: "Connected peers: ",
    local_name: "Local name",
    layout_title: "Screen layout — drag a screen to reposition it",
    layout_hint: "Place screens the way they sit on your desk. The primary (highlighted) is where your real cursor lives; cross an edge to hand control to a neighbour.",
    layout_tip: "Tip: drag a secondary roughly against the primary's edge (small offsets fine); rest or push the mouse into that edge to cross. Orange dot = live cursor.",
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
    ctrl_local_short: "This machine",
    ctrl_remote: "On {} (local cursor hidden; push back toward the primary's edge to return)",
    ctrl_pushing: "Edge push {n}/{total} — keep pushing…",
    hotkey_hint: "Switch-hotkey: Ctrl+Alt+Space or ScrollLock rotates the mouse between the primary and each secondary (works on both sides).",
    diag_hint: "Crossing diagnostics log (paste this file when crossing misbehaves):",
    background_hint: "Closing this window minimises MouseShare to the background and sharing keeps running. Use the button below to quit for real.",
    exit_app: "Quit MouseShare",
    tray_show: "Show MouseShare",
    // ---- Sidebar navigation ----
    nav_group_config: "CONFIGURE",
    nav_group_network: "NETWORK",
    nav_connection: "Connection",
    nav_layout: "Screen layout",
    nav_status: "Status",
    nav_discovered: "Discovered devices",
    // ---- Page headings ----
    page_connection: "Connection",
    page_connection_sub: "Pick this machine's role, then tell secondaries how to reach it",
    page_layout: "Screen layout",
    page_layout_sub: "Arrange the screens the way they sit on your desk · orange dot = live cursor",
    page_status: "Status",
    page_status_sub: "Session stats, network state and the latest control decisions",
    page_discovered: "Discovered devices",
    page_discovered_sub: "Primaries broadcasting a beacon on the LAN — one click to connect",
    // ---- Card headings ----
    card_role: "My role",
    card_role_sub: "Decides who owns the real mouse and keyboard",
    card_primary: "Primary setup",
    card_primary_sub: "How secondaries reach this machine",
    card_secondary: "Secondary setup",
    card_secondary_sub: "Enter the primary's address and port",
    card_screens: "Local screens",
    card_screens_sub: "These join the primary's layout automatically",
    card_canvas: "Virtual desktop",
    card_canvas_sub: "Drag to reposition · orange dot = live cursor",
    card_clients: "Connected clients",
    card_clients_sub: "Drag a screen onto the canvas above to add it",
    card_stats: "This session",
    card_activity: "Recent events",
    card_activity_sub: "Real entries from the crossing diagnostics log",
    card_network: "Network",
    card_network_sub: "Not connected to any primary yet",
    card_discovered: "Primaries on the LAN",
    card_discovered_sub: "Found automatically via the UDP beacon",
    // ---- Role mode cards ----
    role_primary_card: "Primary",
    role_primary_desc:
        "This machine owns the real mouse and keyboard and shares them with every secondary.",
    role_secondary_card: "Secondary",
    role_secondary_desc:
        "Receives input from the primary, or pushes the cursor to a screen edge to take over.",
    // ---- Form / misc ----
    local_ip: "Local IP",
    port_hint: "TCP port secondaries connect to",
    card_speed: "Cross-screen speed",
    card_speed_sub: "Derived automatically from both machines' scale factors; trim if needed",
    speed_multiplier: "Manual trim",
    speed_hint: "1.00 = fully automatic; lower if too fast, higher if too slow",
    speed_effective: "Effective ratio",
    speed_no_peer: "Waiting for a secondary…",
    copy: "Copy",
    screens_empty: "No screens yet.",
    online: "Online",
    offline: "Offline",
    // ---- Status stats ----
    stat_peers: "Connected",
    stat_peers_foot: "secondaries",
    stat_conn: "Connection",
    stat_ctrl: "Mouse control",
    activity_empty:
        "Nothing logged yet. Once you start crossing, decisions appear here in real time.",
    // ---- Permission guidance dialog ----
    perm_title: "Input permission required",
    perm_body: "MouseShare needs two permissions to capture and route input:\n① Input Monitoring — read keyboard & mouse events\n② Accessibility — control the cursor\nIf macOS didn't prompt, or you denied it earlier, use the buttons below to open System Settings, enable MouseShare, then click \"Re-check\".",
    perm_open_input: "Open System Settings · Input Monitoring",
    perm_open_accessibility: "Open System Settings · Accessibility",
    perm_recheck: "I've enabled it — re-check",
    perm_dismiss: "Later",
};

/// Look up the string table for a language.
pub fn tr(lang: Lang) -> Tr {
    match lang {
        Lang::Zh => ZH,
        Lang::En => EN,
    }
}
