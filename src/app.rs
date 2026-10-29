use crate::clipboard::SharedClipboard;
use crate::frame::Canvas;
use crate::net::{self, ControllerHandle, FrameBoard, InputStatusView, LogFn, NetEvent, Peer};
use crate::protocol::{ClientMsg, DEFAULT_PORT};
use crate::theme;
use eframe::egui;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 两次 Esc 之间小于这个间隔就认为是要退出远程控制。
const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(400);
/// 一帧里变化的瓦片超过这个数，直接整帧上传纹理，省掉一堆小上传。
const PARTIAL_UPLOAD_LIMIT: usize = 32;

// ---- 键鼠自愈的时间常量 ----

/// 每 500 ms 找被控端要一次键鼠通道状态。
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
/// 用户一直在操作，但被控端计数这么久没涨 → 判定通道堵塞。
const STALL_WINDOW: Duration = Duration::from_millis(1500);
/// 判定「用户正在操作」的窗口。
const ACTIVE_WINDOW: Duration = Duration::from_secs(2);
/// 两次自动修复之间的最小间隔，避免抖动时疯狂重连。
const REPAIR_COOLDOWN: Duration = Duration::from_secs(4);
/// 修复之后给对端留的确认时间，这段时间内不再判故障。
const REPAIR_GRACE: Duration = Duration::from_millis(1200);
/// 软修复连续无效这么多次，就升级成硬重连。
const REPAIR_BEFORE_RECONNECT: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Agent,
    Controller,
}

/// 键鼠通道当前的问题类型。
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputProblem {
    /// 指令发出去了，但被控端的计数不涨 —— 通道被堵住（线程卡死 / 半死 TCP）。
    Stalled,
    /// 被控端收到了，但 SendInput 被系统拒绝（UIPI / UAC 安全桌面 / 锁屏）。
    Rejected,
}

/// 「键鼠到底通不通」的自检状态机。
///
/// 上一版没有任何反馈：指令发出去就完了，被控端有没有收到、有没有注入成功，
/// 控制端一无所知，用户只能感觉到「有时候突然就不动了」。
/// 现在每 500 ms 要一次计数，用户一操作就能发现计数不涨 / 注入被拒，
/// 然后自动修复（先软修复，无效再硬重连）。
#[derive(Default)]
struct InputHealth {
    status: Option<InputStatusView>,
    last_probe: Option<Instant>,
    /// 最近一次「本地确实发了键鼠」的时刻。
    last_local_input: Option<Instant>,
    /// 上次看到的被控端计数 + 它最后一次变化的时刻。
    last_received: u64,
    /// 被控端当前会话编号（换会话说明旧连接已被顶掉，计数要重新开始）。
    session: Option<u64>,
    last_change: Option<Instant>,
    problem: Option<InputProblem>,
    /// 连续几次探测都「注入被拒」。偶发一次 SendInput 失败不值得弹红条，
    /// 连续两次才算数。
    bad_streak: u32,
    /// 软修复次数（成功确认后清零）。
    repairs: u32,
    /// 已经强制重连过几次。用来兜住「修不好就无限重连」的死循环。
    hard_reconnects: u32,
    /// 自动修复放弃（重连也没用），等用户按 F8/F9 再试。
    gave_up: bool,
    last_repair: Option<Instant>,
    grace_until: Option<Instant>,
    /// 刚修好，用来闪一下绿条。
    recovered_at: Option<Instant>,
}

impl InputHealth {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn mark_local_input(&mut self) {
        self.last_local_input = Some(Instant::now());
    }

    fn on_status(&mut self, view: InputStatusView) {
        // 换了会话（旧连接被顶掉 / 刚重连）：被控端计数从 0 开始，旧值不能再当参考，
        // 否则「消息其实没过去」会被误判成正常。
        if self.session != Some(view.session) {
            self.session = Some(view.session);
            self.last_received = view.received;
            self.last_change = None;
            self.problem = None;
            self.repairs = 0;
            self.bad_streak = 0;
            self.hard_reconnects = 0;
            self.gave_up = false;
            self.grace_until = None;
            self.recovered_at = Some(Instant::now());
            self.status = Some(view);
            return;
        }
        let advanced = self.status.is_none() || view.received != self.last_received;
        if advanced {
            self.last_received = view.received;
            self.last_change = Some(Instant::now());
            self.bad_streak = 0;
            self.hard_reconnects = 0;
            self.gave_up = false;
            if self.problem.is_some() {
                self.problem = None;
                self.recovered_at = Some(Instant::now());
                self.repairs = 0;
            }
        }
        self.status = Some(view);
    }

    fn active(&self) -> bool {
        self.last_local_input
            .map(|at| at.elapsed() < ACTIVE_WINDOW)
            .unwrap_or(false)
    }

    fn stalled(&self) -> bool {
        let (Some(change), Some(local)) = (self.last_change, self.last_local_input) else {
            return false;
        };
        // 只有在「用户在被控端最后一次确认之后又发过指令」时才判堵住：
        // 挂机、按住键不动都不会有新指令，也就不会误报。
        local > change && change.elapsed() >= STALL_WINDOW
    }
}

pub struct App {
    mode: Mode,
    clipboard: SharedClipboard,
    clipboard_enabled: bool,
    target: String,
    controller: Option<ControllerHandle>,
    peers: Vec<Peer>,
    scan_rx: Option<Receiver<Vec<Peer>>>,
    scanning: bool,
    remote_tex: Option<egui::TextureHandle>,
    last_frame_seq: u64,
    /// 远端当前的键鼠状态镜像；`control_enabled == false` 时始终是空的。
    input: RemoteInput,
    /// 是否把本地键鼠转发给远端（点画面启用，点「释放键鼠」关闭）。
    input_armed: bool,
    /// 用户刚点过本窗口。egui 的 `InputState::focused` 在 Windows 上偶尔会卡在
    /// false（切全屏、UAC 弹窗、别的置顶窗口抢焦点之后），一旦卡住，上一版就直接
    /// `return` 掉全部键鼠 —— 断线重连也不会重置它，这正是「重连也没用」的原因。
    /// 现在用这个闩锁兜底：只要用户在本窗口里点过，就当它有焦点。
    focus_latched: bool,
    /// 目标输入框是否正在编辑。编辑时不转发键盘，免得在 IP 框里打字打到远端去。
    target_editing: bool,
    release_keyboard_requested: bool,
    /// 「只看画面」开关：false 时不往被控端发任何键鼠消息。
    control_enabled: bool,
    resend_input_state: bool,
    health: InputHealth,
    logs: Arc<Mutex<Vec<String>>>,
    status: String,
    quality: u8,
    fps: u8,
    max_width: u32,
    last_quality: (u8, u8, u32),
    agent: Option<net::AgentHandle>,
    autostart: bool,
    firewall_note: String,
    fullscreen: bool,
    ctx: Option<egui::Context>,
    last_escape: Option<Instant>,
    exit_requested: bool,
    escape_hint_until: Option<Instant>,
    stats: NetStatsView,
    /// 界面右下角的临时提示：(文字, 颜色, 到期时间)。
    toast: Option<(String, egui::Color32, Instant)>,
}

/// 一次远控会话里「远端现在按着什么」的镜像。
///
/// 所有按键动作都只根据 egui 的**事件**推进，而不是每帧轮询 `button_down()`：
/// 指针在画面外按下的事件会被整条丢弃，抬起事件只对「确实转发过按下」的键生效，
/// 因此不会出现「点别的地方，被控端却右击一下」这种幽灵操作，也不会把按键
/// 卡在按下状态。
///
/// `mods` 单独跟踪，是因为 egui 的 `Key` 枚举里根本没有 Ctrl/Shift/Alt，
/// 修饰键只能靠每帧的 `modifiers` 快照同步 —— 而这正是 Ctrl+C 有时丢 Ctrl 的原因。
#[derive(Default)]
struct RemoteInput {
    buttons: [bool; 3],
    mods: [bool; 3],
    keys: Vec<u16>,
    last_norm: Option<(f32, f32)>,
    wheel_accum: f32,
}

impl RemoteInput {
    fn reset(&mut self) {
        self.buttons = [false; 3];
        self.mods = [false; 3];
        self.keys.clear();
        self.last_norm = None;
        self.wheel_accum = 0.0;
    }

    fn held(&self) -> bool {
        self.buttons.iter().any(|b| *b) || self.mods.iter().any(|m| *m) || !self.keys.is_empty()
    }

    fn press_key(&mut self, vk: u16) {
        if self.keys.len() < 24 && !self.keys.contains(&vk) {
            self.keys.push(vk);
        }
    }

    fn release_key(&mut self, vk: u16) -> bool {
        match self.keys.iter().position(|k| *k == vk) {
            Some(index) => {
                self.keys.remove(index);
                true
            }
            None => false,
        }
    }
}

/// 控制端自己算出来的画质/延迟统计，用来判断「卡」到底是网络还是抓屏。
#[derive(Default)]
struct NetStatsView {
    at: Option<Instant>,
    frames: u64,
    bytes: u64,
    fps: f32,
    mbps: f32,
    rtt_ms: u32,
    decode_ms: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, mode: Mode) -> Self {
        theme::apply(&cc.egui_ctx);
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let log_fn: LogFn = {
            let logs = logs.clone();
            Arc::new(move |msg: String| push_log(&logs, msg))
        };
        let clipboard = SharedClipboard::new();
        let status: String;
        let agent = if mode == Mode::Agent {
            match net::start_agent(DEFAULT_PORT, clipboard.clone(), log_fn, true) {
                Ok(handle) => {
                    status = format!(
                        "被控端已启动，监听端口 {}。请把本机 IP 告诉控制端。",
                        DEFAULT_PORT
                    );
                    Some(handle)
                }
                Err(e) => {
                    status = format!("被控端启动失败: {e:#}");
                    None
                }
            }
        } else {
            status = "输入被控端 IP 后连接，或点击「扫描局域网」。".to_string();
            None
        };
        push_log(&logs, "LanLink 启动".to_string());
        push_log(&logs, status.clone());
        let mut app = Self {
            mode,
            clipboard,
            clipboard_enabled: true,
            target: std::env::var("LANLINK_TARGET").unwrap_or_default(),
            controller: None,
            peers: Vec::new(),
            scan_rx: None,
            scanning: false,
            remote_tex: None,
            last_frame_seq: 0,
            input: RemoteInput::default(),
            input_armed: true,
            focus_latched: false,
            target_editing: false,
            release_keyboard_requested: false,
            control_enabled: true,
            resend_input_state: false,
            health: InputHealth::default(),
            logs,
            status,
            quality: 72,
            fps: 60,
            max_width: 1920,
            last_quality: (0, 0, 0),
            agent,
            autostart: net::autostart_enabled(),
            firewall_note: String::new(),
            fullscreen: false,
            ctx: Some(cc.egui_ctx.clone()),
            last_escape: None,
            exit_requested: false,
            escape_hint_until: None,
            stats: NetStatsView::default(),
            toast: None,
        };
        // 开发/自测用：设了 LANLINK_TARGET 就直接连过去，省得每次手点。
        if !app.target.is_empty() {
            app.connect();
        }
        app
    }

    fn log(&self, msg: impl Into<String>) {
        push_log(&self.logs, msg.into());
    }

    fn toast(&mut self, text: impl Into<String>, color: egui::Color32) {
        self.toast = Some((text.into(), color, Instant::now() + Duration::from_secs(4)));
    }

    fn poll_controller_events(&mut self, ctx: &egui::Context) {
        let Some(controller) = self.controller.as_ref() else {
            return;
        };
        let events: Vec<NetEvent> = controller.events.try_iter().collect();
        let frames = controller.frames.clone();
        let has_frame = frames.seq() != self.last_frame_seq;
        if has_frame {
            self.last_frame_seq = frames.seq();
        }
        for event in events {
            match event {
                NetEvent::Connected(hello) => {
                    self.status = format!(
                        "已连接 {} ({}x{})",
                        hello.host, hello.width, hello.height
                    );
                    self.remote_tex = None;
                    self.last_frame_seq = 0;
                    self.last_quality = (0, 0, 0);
                    self.input.reset();
                    self.health.reset();
                    self.resend_input_state = true;
                    self.stats = NetStatsView::default();
                    // 新连接：把当前剪贴板补推一次，否则 Ctrl+V 会粘到重连前的旧内容。
                    self.clipboard.invalidate();
                    self.log(format!("已连接到 {} ({})", hello.host, hello.width));
                }
                NetEvent::Disconnected(reason) => {
                    self.status = format!("连接断开：{reason}，正在自动重连...");
                    self.remote_tex = None;
                    self.input.reset();
                    self.health.reset();
                    self.stats = NetStatsView::default();
                    self.log(self.status.clone());
                }
                NetEvent::Log(text) => {
                    self.log(text);
                }
                NetEvent::Clipboard(payload) => {
                    if self.clipboard_enabled {
                        match self.clipboard.apply(&payload) {
                            Ok(()) => self.log("剪贴板已同步"),
                            Err(e) => self.log(format!("剪贴板写入失败: {e:#}")),
                        }
                    }
                }
                NetEvent::FileSaved(path, bytes) => {
                    self.status = format!("文件已保存：{path}（{bytes} 字节）");
                    self.log(self.status.clone());
                }
                NetEvent::Notice(text) => {
                    self.status = text.clone();
                    self.log(text);
                }
                NetEvent::InputStatus(view) => {
                    self.health.on_status(view);
                }
            }
        }
        if has_frame {
            self.upload_frame(ctx, &frames);
        }
        if self.resend_input_state {
            self.resend_input_state = false;
            if let Some(controller) = self.controller.as_ref() {
                controller.set_input(self.control_enabled);
            }
        }
    }

    /// 把画布上的更新搬进纹理。只上传变化的瓦片，避免每帧 8 MB 的整屏拷贝/上传。
    fn upload_frame(&mut self, ctx: &egui::Context, frames: &Arc<FrameBoard>) {
        let mut texture = self.remote_tex.take();
        frames.take_update(|canvas, patches, full| {
            let size = [canvas.grid.width as usize, canvas.grid.height as usize];
            let reuse = match texture.as_ref() {
                Some(tex) => tex.size() == size && !full,
                None => false,
            };
            if !reuse {
                texture = Some(ctx.load_texture(
                    "remote-screen",
                    color_image_full(canvas),
                    egui::TextureOptions::LINEAR,
                ));
                return;
            }
            let Some(tex) = texture.as_mut() else {
                return;
            };
            if patches.len() > PARTIAL_UPLOAD_LIMIT {
                tex.set(color_image_full(canvas), egui::TextureOptions::LINEAR);
                return;
            }
            for &(x, y, w, h) in patches {
                tex.set_partial(
                    [x as usize, y as usize],
                    color_image_rect(canvas, x, y, w, h),
                    egui::TextureOptions::LINEAR,
                );
            }
        });
        self.remote_tex = texture;
    }

    fn refresh_stats(&mut self) {
        let Some(controller) = self.controller.as_ref() else {
            self.stats = NetStatsView::default();
            return;
        };
        let counters = controller.frames.stats();
        let frames = counters.frames.load(Ordering::Relaxed);
        let bytes = counters.bytes.load(Ordering::Relaxed);
        let now = Instant::now();
        let window = self
            .stats
            .at
            .map(|at| now.duration_since(at).as_secs_f32())
            .unwrap_or(0.0);
        if window >= 0.25 {
            let df = frames.saturating_sub(self.stats.frames);
            let db = bytes.saturating_sub(self.stats.bytes);
            self.stats.fps = df as f32 / window;
            self.stats.mbps = db as f32 / window / (1024.0 * 1024.0);
            self.stats.frames = frames;
            self.stats.bytes = bytes;
            self.stats.at = Some(now);
        } else if self.stats.at.is_none() {
            self.stats.at = Some(now);
            self.stats.frames = frames;
            self.stats.bytes = bytes;
        }
        self.stats.rtt_ms = counters.rtt_us.load(Ordering::Relaxed) / 1000;
        self.stats.decode_ms = counters.decode_us.load(Ordering::Relaxed) as f32 / 1000.0;
    }

    // ---- 键鼠通道自愈 ----

    /// 每帧跑一次：探活 → 判故障 → 自动修复。
    fn poll_input_health(&mut self) {
        let connected = self
            .controller
            .as_ref()
            .map(|c| c.is_connected())
            .unwrap_or(false);
        if !connected {
            self.health.reset();
            return;
        }
        let now = Instant::now();
        if self
            .health
            .last_probe
            .map(|at| now.duration_since(at) >= PROBE_INTERVAL)
            .unwrap_or(true)
        {
            if let Some(controller) = self.controller.as_ref() {
                controller.probe_input();
            }
            self.health.last_probe = Some(now);
        }
        if let Some(grace) = self.health.grace_until {
            if now >= grace {
                self.health.grace_until = None;
            } else {
                return;
            }
        }
        if self.health.recovered_at.take().is_some() {
            self.toast("键鼠通道已恢复", theme::OK);
            self.log("键鼠通道已自动恢复");
        }
        let Some(status) = self.health.status.clone() else {
            return;
        };
        // 只有用户确实在操作的时候才判故障，避免挂机时误报。
        if !self.health.active() {
            if self.health.problem == Some(InputProblem::Stalled) {
                self.health.problem = None;
            }
            return;
        }
        let problem = if status.gated || !self.control_enabled {
            // 被控端说「只看画面」，是我们自己让的，不算故障。
            None
        } else if !status.ok {
            self.health.bad_streak = self.health.bad_streak.saturating_add(1);
            if self.health.bad_streak >= 2 {
                Some(InputProblem::Rejected)
            } else {
                None
            }
        } else if self.health.stalled() {
            Some(InputProblem::Stalled)
        } else {
            None
        };
        let changed = problem != self.health.problem;
        self.health.problem = problem;
        if let (Some(problem), true) = (problem, changed) {
            let text = match problem {
                InputProblem::Stalled => "键鼠通道无响应：指令发出去了，但被控端没有确认收到",
                InputProblem::Rejected => {
                    "被控端无法注入键鼠：可能焦点在管理员窗口 / UAC 安全桌面 / 锁屏界面"
                }
            };
            self.log(text);
            self.set_status(text);
        }
        if self.health.problem.is_some() && !self.health.gave_up {
            let due = self
                .health
                .last_repair
                .map(|at| now.duration_since(at) >= REPAIR_COOLDOWN)
                .unwrap_or(true);
            if due {
                self.repair_input(false);
            }
        }
    }

    /// 键鼠修复：先软修（重发状态 + 让被控端松开所有键），无效再硬重连。
    fn repair_input(&mut self, manual: bool) {
        if self.controller.is_none() {
            return;
        }
        if manual {
            self.health.repairs = 0;
            self.health.hard_reconnects = 0;
            self.health.gave_up = false;
        }
        self.health.repairs = self.health.repairs.saturating_add(1);
        if self.health.repairs > REPAIR_BEFORE_RECONNECT {
            self.health.repairs = 0;
            self.health.hard_reconnects = self.health.hard_reconnects.saturating_add(1);
            // 重连三次还修不好，多半不是通道的问题（被控端在安全桌面/锁屏，
            // 或者目标窗口权限更高）。这时候继续自动重连只会没完没了地闪断，
            // 停下来把话讲清楚，让用户按 F8 / F9 再试。
            if self.health.hard_reconnects > 3 {
                self.health.gave_up = true;
                let text = "自动修复已放弃：重连 3 次仍无法注入键鼠，请检查被控端是否在锁屏 / UAC 安全桌面，或目标窗口是否以更高权限运行";
                self.log(text);
                self.set_status(text);
                self.toast("自动修复已放弃，按 F8 可再试一次", theme::ERR);
                return;
            }
            if let Some(controller) = self.controller.as_ref() {
                controller.reconnect_now();
            }
            let text = "软修复无效，已强制重新连接（被控端会顶掉旧会话）";
            self.log(text);
            self.set_status(text);
            self.toast("已强制重新连接", theme::WARN);
            return;
        }
        self.release_remote_input();
        self.input.reset();
        if let Some(controller) = self.controller.as_ref() {
            controller.resync_input();
            controller.set_input(self.control_enabled);
            controller.probe_input();
        }
        let now = Instant::now();
        self.health.problem = None;
        self.health.last_repair = Some(now);
        self.health.grace_until = Some(now + REPAIR_GRACE);
        self.health.last_change = None;
        let text = if manual {
            "已执行键鼠修复（被控端已松开全部按键并重开注入通道）"
        } else {
            "检测到键鼠通道异常，已自动修复"
        };
        self.log(text);
        self.set_status(text);
        self.toast(text, theme::WARN);
    }

    fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
    }

    fn send_quality_if_needed(&mut self) {
        let wanted = (self.quality, self.fps, self.max_width);
        if wanted == self.last_quality {
            return;
        }
        if let Some(controller) = self.controller.as_ref() {
            if controller.is_connected() {
                controller.set_quality(wanted.0, wanted.1, wanted.2);
                self.last_quality = wanted;
            }
        }
    }

    fn parse_target(&self) -> Result<SocketAddr, String> {
        let text = self.target.trim();
        if text.is_empty() {
            return Err("请输入被控端 IP".to_string());
        }
        if let Ok(addr) = text.parse::<SocketAddr>() {
            return Ok(addr);
        }
        let ip: IpAddr = text
            .parse()
            .map_err(|_| format!("无法识别地址：{text}"))?;
        Ok(SocketAddr::new(ip, DEFAULT_PORT))
    }

    fn connect(&mut self) {
        let addr = match self.parse_target() {
            Ok(a) => a,
            Err(e) => {
                self.status = e.clone();
                self.log(e);
                return;
            }
        };
        if let Some(old) = self.controller.take() {
            old.disconnect();
        }
        let wake = self.ctx.as_ref().map(|ctx| {
            let ctx = ctx.clone();
            Arc::new(move || ctx.request_repaint()) as Arc<dyn Fn() + Send + Sync>
        });
        match net::connect_controller(addr, self.clipboard.clone(), wake) {
            Ok(handle) => {
                self.controller = Some(handle);
                self.status = format!("正在连接 {addr} ...");
                self.remote_tex = None;
                self.last_frame_seq = 0;
                self.last_quality = (0, 0, 0);
                self.input.reset();
                self.health.reset();
                self.input_armed = true;
                self.focus_latched = false;
                self.stats = NetStatsView::default();
                self.log(self.status.clone());
            }
            Err(e) => {
                self.status = format!("连接失败: {e:#}");
                self.log(self.status.clone());
            }
        }
    }

    fn disconnect(&mut self) {
        self.release_remote_input();
        if let Some(controller) = self.controller.take() {
            controller.frames.clear();
            controller.disconnect();
        }
        self.remote_tex = None;
        self.last_frame_seq = 0;
        self.stats = NetStatsView::default();
        self.health.reset();
        self.focus_latched = false;
        self.status = "已断开".to_string();
        self.log("已主动断开");
    }

    /// 把远端可能还按着的键 / 鼠标键全部放开（断开、失焦、切到只看画面、退出时都要）。
    fn release_remote_input(&mut self) {
        let Some(controller) = self.controller.as_ref() else {
            self.input.reset();
            return;
        };
        for index in 0..3 {
            if self.input.buttons[index] {
                self.input.buttons[index] = false;
                controller.send(ClientMsg::MouseButton {
                    button: index as u8,
                    down: false,
                });
            }
        }
        for (index, vk) in [0xA2u16, 0xA0, 0xA4].into_iter().enumerate() {
            if self.input.mods[index] {
                self.input.mods[index] = false;
                controller.send(ClientMsg::Key { vk, down: false });
            }
        }
        let keys: Vec<u16> = self.input.keys.drain(..).collect();
        for vk in keys {
            controller.send(ClientMsg::Key { vk, down: false });
        }
        self.input.last_norm = None;
        self.input.wheel_accum = 0.0;
    }

    /// 切换「允许远程操作 / 只看画面」。
    fn set_control_enabled(&mut self, enabled: bool) {
        if self.control_enabled == enabled {
            return;
        }
        if !enabled {
            self.release_remote_input();
        }
        self.control_enabled = enabled;
        if let Some(controller) = self.controller.as_ref() {
            controller.set_input(enabled);
        }
        let text = if enabled {
            "已恢复远程操作"
        } else {
            "已切换为「只看画面」，不再向被控端发送键鼠"
        };
        self.status = text.to_string();
        self.log(text);
    }

    fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        self.status = "正在扫描局域网...".to_string();
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        std::thread::spawn(move || {
            let peers = net::scan_once(Duration::from_millis(1600));
            let _ = tx.send(peers);
        });
    }

    fn poll_scan(&mut self) {
        let result = self.scan_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(peers) = result {
            self.peers = peers;
            self.scanning = false;
            self.scan_rx = None;
            self.status = format!("扫描完成，发现 {} 台设备", self.peers.len());
        }
    }

    fn send_file_dialog(&mut self) {
        let info = self
            .controller
            .as_ref()
            .filter(|c| c.is_connected())
            .map(|c| (c.addr, c.event_tx.clone()));
        let Some((addr, event_tx)) = info else {
            self.status = "请先连接被控端".to_string();
            return;
        };
        if let Some(path) = rfd::FileDialog::new().pick_file() {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file")
                .to_string();
            self.status = format!("正在发送 {name} ...");
            self.log(self.status.clone());
            net::send_file(addr, path, event_tx);
        }
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
    }

    // ---- 界面 ----

    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let connected = self
            .controller
            .as_ref()
            .map(|c| c.is_connected())
            .unwrap_or(false);
        let is_controller = self.mode == Mode::Controller;

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("LanLink")
                    .size(20.0)
                    .strong()
                    .color(theme::ACCENT),
            );
            ui.label(
                egui::RichText::new("局域网远控")
                    .size(12.5)
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(if is_controller { "控制端" } else { "被控端" })
                    .size(13.0)
                    .color(theme::TEXT_DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("v{}", crate::protocol::APP_VERSION))
                        .size(12.0)
                        .color(theme::TEXT_DIM),
                );
                if is_controller {
                    let (dot, text, fill) = if !self.control_enabled {
                        (theme::WARN, "只看画面", theme::WARN.gamma_multiply(0.16))
                    } else if !self.input_armed {
                        (
                            theme::WARN,
                            "键鼠已释放 · 点画面启用",
                            theme::WARN.gamma_multiply(0.16),
                        )
                    } else if connected {
                        (theme::OK, "键鼠已启用", theme::OK.gamma_multiply(0.16))
                    } else {
                        (theme::TEXT_DIM, "键鼠未连接", theme::TEXT_DIM.gamma_multiply(0.14))
                    };
                    theme::pill(ui, dot, text, fill);
                    ui.add_space(4.0);
                }
                let (dot, text, fill) = if self.clipboard_enabled {
                    (theme::OK, "剪贴板 开", theme::CARD)
                } else {
                    (theme::TEXT_DIM, "剪贴板 关", theme::CARD)
                };
                theme::pill(ui, dot, text, fill);
            });
        });

        if !is_controller {
            return;
        }

        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("目标").color(theme::TEXT_DIM));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.target)
                    .hint_text("192.168.1.23 或 IP:端口")
                    .desired_width(190.0)
                    .font(egui::TextStyle::Body),
            );
            self.target_editing = resp.has_focus();

            if self.controller.is_none() {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("连接").strong(),
                    ))
                    .clicked()
                {
                    self.connect();
                }
            } else {
                if ui.button("断开").clicked() {
                    self.disconnect();
                }
                let can = connected;
                if ui
                    .add_enabled(
                        can,
                        egui::Button::new(egui::RichText::new("立即重连").color(theme::TEXT)),
                    )
                    .on_hover_text("立刻重连一次（被控端会把旧会话顶掉，状态一定是干净的）")
                    .clicked()
                {
                    if let Some(controller) = self.controller.as_ref() {
                        controller.reconnect_now();
                    }
                    self.input.reset();
                    self.health.reset();
                    self.log("手动重连");
                }
            }
            if ui
                .add_enabled(
                    !self.scanning,
                    egui::Button::new(if self.scanning {
                        "扫描中..."
                    } else {
                        "扫描局域网"
                    }),
                )
                .clicked()
            {
                self.start_scan();
            }

            ui.separator();

            let (label, color) = if self.control_enabled {
                ("控制中：允许操作", theme::OK)
            } else {
                ("只看画面：不发送键鼠", theme::WARN)
            };
            let button = egui::Button::new(egui::RichText::new(label).color(color).strong())
                .selected(!self.control_enabled);
            if ui
                .add(button)
                .on_hover_text("点击切换「只看画面」（不向被控端发送任何键鼠）")
                .clicked()
            {
                self.set_control_enabled(!self.control_enabled);
            }
            let mut clip = self.clipboard_enabled;
            if ui.checkbox(&mut clip, "剪贴板同步").changed() {
                self.clipboard_enabled = clip;
                self.clipboard.set_enabled(clip);
            }
        });

        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("画质").color(theme::TEXT_DIM));
            ui.add(
                egui::Slider::new(&mut self.quality, 20..=95)
                    .show_value(true)
                    .trailing_fill(true),
            );
            ui.add_space(6.0);
            ui.label(egui::RichText::new("帧率").color(theme::TEXT_DIM));
            ui.add(egui::Slider::new(&mut self.fps, 1..=60).suffix(" fps"));
            ui.add_space(6.0);
            ui.label(egui::RichText::new("最大宽度").color(theme::TEXT_DIM));
            egui::ComboBox::from_id_salt("max_width")
                .selected_text(format!("{} px", self.max_width))
                .show_ui(ui, |ui| {
                    for width in [1280u32, 1600, 1920, 2560, 3840] {
                        ui.selectable_value(&mut self.max_width, width, format!("{width} px"));
                    }
                });
            ui.separator();
            if ui
                .add_enabled(connected, egui::Button::new("发送文件"))
                .clicked()
            {
                self.send_file_dialog();
            }
            if ui
                .button(if self.fullscreen {
                    "退出全屏 (F11)"
                } else {
                    "全屏 (F11)"
                })
                .clicked()
            {
                self.toggle_fullscreen(ctx);
            }
            if ui
                .button("释放键鼠")
                .on_hover_text("立刻放开被控端上所有按住的键，并停止转发键盘")
                .clicked()
            {
                self.release_keyboard_requested = true;
            }
            if ui
                .add_enabled(
                    connected,
                    egui::Button::new(
                        egui::RichText::new("修复键鼠 (F8)").color(theme::WARN).strong(),
                    ),
                )
                .on_hover_text(
                    "键鼠突然失灵时点这个：让被控端松开所有键并重开注入通道，1.2 秒无改善会自动强制重连",
                )
                .clicked()
            {
                self.repair_input(true);
            }
        });
    }

    fn controller_ui(&mut self, ui: &mut egui::Ui) {
        if self.controller.is_none() {
            self.welcome_ui(ui);
            return;
        }
        let connected = self
            .controller
            .as_ref()
            .map(|c| c.is_connected())
            .unwrap_or(false);
        if !connected {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.spinner();
                ui.add_space(6.0);
                ui.label(egui::RichText::new("正在连接 / 等待自动重连...").size(16.0));
                ui.add_space(8.0);
                if ui.button("停止重连").clicked() {
                    self.disconnect();
                }
            });
            return;
        }

        // 键鼠通道异常时，在画面上方挂一条醒目的横幅。
        if let Some(problem) = self.health.problem {
            self.problem_banner(ui, problem);
            ui.add_space(4.0);
        }

        let available = ui.available_size();
        let (area, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
        ui.painter()
            .rect_filled(area, egui::CornerRadius::ZERO, egui::Color32::from_rgb(8, 9, 12));
        let Some(texture) = self.remote_tex.as_ref() else {
            ui.painter().text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                "等待远程画面...",
                egui::FontId::proportional(20.0),
                theme::TEXT_DIM,
            );
            return;
        };
        let tex_size = texture.size_vec2();
        if tex_size.x <= 1.0 || tex_size.y <= 1.0 {
            return;
        }
        let scale = (area.width() / tex_size.x).min(area.height() / tex_size.y);
        let draw_size = tex_size * scale;
        let image_rect = egui::Rect::from_center_size(area.center(), draw_size);
        ui.painter().image(
            texture.id(),
            image_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );

        // 在画面里点一下 = 主动接管键鼠。
        let window_focused = ui.input(|i| i.focused);
        if response.clicked() || response.drag_started() || response.is_pointer_button_down_on() {
            self.input_armed = true;
            if !window_focused {
                // egui 的 focused 标志卡住了：点一下本窗口通常能让它自己恢复，
                // 恢复不了也没关系，下面用 focus_latched 兜底。
                self.focus_latched = true;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        if window_focused {
            // 真实信号优先，闩锁自动解除。
            self.focus_latched = false;
        }

        if self.release_keyboard_requested {
            response.surrender_focus();
            self.release_remote_input();
            self.input.reset();
            self.input_armed = false;
            self.release_keyboard_requested = false;
            self.toast("已释放键鼠，点击画面重新启用", theme::WARN);
        }

        self.remote_input(ui, &response, image_rect);

        if !self.control_enabled {
            draw_badge(
                ui,
                image_rect.min + egui::vec2(12.0, 12.0),
                "只看画面 · 不发送键鼠",
                theme::WARN,
            );
        } else if !self.input_armed {
            draw_badge(
                ui,
                image_rect.min + egui::vec2(12.0, 12.0),
                "在画面里点一下即可接管键鼠",
                theme::ACCENT,
            );
        }
        if self.escape_hint_active() {
            draw_badge(
                ui,
                egui::pos2(image_rect.center().x, image_rect.min.y + 40.0),
                "再按一次 Esc 退出远程控制",
                egui::Color32::WHITE,
            );
        }
    }

    fn problem_banner(&mut self, ui: &mut egui::Ui, problem: InputProblem) {
        let (title, detail) = match problem {
            InputProblem::Stalled => (
                "键鼠通道无响应",
                "指令已发出，但被控端没有确认收到。按 F8 立即修复，几秒内会自动重连。",
            ),
            InputProblem::Rejected => (
                "被控端无法注入键鼠",
                "被控端收到了指令但被系统拒绝：可能焦点在管理员窗口 / UAC 安全桌面 / 锁屏。\
                 在被控端关掉 UAC 窗口或解锁后，按 F8 重新接管。",
            ),
        };
        let (title, detail) = if self.health.gave_up {
            (
                "键鼠仍无法注入",
                "自动修复已经试过 3 次重连，还是不行。请到被控端确认没有停在锁屏 / \
                 UAC 安全桌面，且目标窗口权限不高于 LanLink；之后按 F8 再试。",
            )
        } else {
            (title, detail)
        };
        egui::Frame::default()
            .fill(egui::Color32::from_rgb(58, 26, 30))
            .stroke(egui::Stroke::new(1.0, theme::ERR))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(format!("⚠ {title}"))
                            .color(theme::ERR)
                            .strong()
                            .size(16.0),
                    );
                    ui.label(egui::RichText::new(detail).color(theme::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new("修复 (F8)").strong(),
                            ))
                            .clicked()
                        {
                            self.repair_input(true);
                        }
                        if ui.button("重连 (F9)").clicked() {
                            if let Some(controller) = self.controller.as_ref() {
                                controller.reconnect_now();
                            }
                        }
                    });
                });
            });
    }

    fn remote_input(&mut self, ui: &egui::Ui, response: &egui::Response, image_rect: egui::Rect) {
        // 双击 Esc 退出远程控制：第一次正常转发给远端，第二次（400 ms 内）本地退出。
        let mut swallow_escape = false;
        if response.has_focus() || self.fullscreen || self.input_armed {
            let presses = ui.input(|i| {
                i.events
                    .iter()
                    .filter(|e| {
                        matches!(
                            e,
                            egui::Event::Key {
                                key: egui::Key::Escape,
                                pressed: true,
                                repeat: false,
                                ..
                            }
                        )
                    })
                    .count()
            });
            if presses > 0 {
                let now = Instant::now();
                for _ in 0..presses {
                    let is_double = self
                        .last_escape
                        .map(|last| now.duration_since(last) <= DOUBLE_ESC_WINDOW)
                        .unwrap_or(false);
                    if is_double {
                        self.last_escape = None;
                        self.escape_hint_until = None;
                        self.exit_requested = true;
                        swallow_escape = true;
                    } else {
                        self.last_escape = Some(now);
                        self.escape_hint_until = Some(now + DOUBLE_ESC_WINDOW);
                    }
                }
                if self.exit_requested {
                    ui.ctx().request_repaint();
                    return;
                }
            }
        }

        let (events, pointer, window_focused) =
            ui.input(|i| (i.events.clone(), i.pointer.interact_pos(), i.focused));
        // 真实焦点信号，或者「用户刚点过本窗口」的闩锁。
        let focus_ok = window_focused || self.focus_latched;

        // 只看画面：把远端所有按下的键鼠放掉，然后一个事件都不转发。
        if !self.control_enabled {
            self.release_remote_input();
            return;
        }
        // 失焦：只把按住的键放掉，**不再阻断**后续处理。
        //
        // 上一版在这里直接 `return`，而 `InputState::focused` 一旦卡在 false，
        // 键盘和鼠标就一起废掉，且断线重连不会重置它 —— 这就是「突然动不了、
        // 重连也没用」的直接原因。现在鼠标继续转发（反正没焦点也收不到事件），
        // 键盘则用 focus_latched 兜底，卡住的标志最多影响一帧。
        if !focus_ok && self.input.held() {
            self.release_remote_input();
        }

        let Some(controller) = self.controller.as_ref() else {
            return;
        };
        if !controller.is_connected() {
            return;
        }

        let over = pointer.map(|p| image_rect.contains(p)).unwrap_or(false);
        let mut sent_input = false;

        // ---- 移动：只在画面内发送；离开画面就清掉参考点，回来时一定重发一次定位 ----
        if let Some(p) = pointer {
            if over {
                let (nx, ny) = normalize(p, image_rect);
                let precise = match self.input.last_norm {
                    Some((lx, ly)) => {
                        (nx - lx).abs() * image_rect.width() > 0.5
                            || (ny - ly).abs() * image_rect.height() > 0.5
                    }
                    None => true,
                };
                if precise {
                    self.input.last_norm = Some((nx, ny));
                    controller.send(ClientMsg::MouseMove { x: nx, y: ny });
                    sent_input = true;
                }
            } else {
                self.input.last_norm = None;
            }
        } else {
            self.input.last_norm = None;
        }

        // ---- 鼠标按键：只认「按下位置在画面里」的事件 ----
        for event in &events {
            let egui::Event::PointerButton {
                pos,
                button,
                pressed,
                ..
            } = event
            else {
                continue;
            };
            let Some(index) = button_index(*button) else {
                continue;
            };
            if *pressed {
                if !image_rect.contains(*pos) {
                    continue;
                }
                // 点之前先精确落位，避免被控端把这次点击算在上一个位置。
                let (nx, ny) = normalize(*pos, image_rect);
                let moved = match self.input.last_norm {
                    Some((lx, ly)) => (nx - lx).abs() > 1e-5 || (ny - ly).abs() > 1e-5,
                    None => true,
                };
                if moved {
                    self.input.last_norm = Some((nx, ny));
                    controller.send(ClientMsg::MouseMove { x: nx, y: ny });
                }
                if !self.input.buttons[index] {
                    self.input.buttons[index] = true;
                    controller.send(ClientMsg::MouseButton {
                        button: index as u8,
                        down: true,
                    });
                    sent_input = true;
                }
            } else if self.input.buttons[index] {
                self.input.buttons[index] = false;
                controller.send(ClientMsg::MouseButton {
                    button: index as u8,
                    down: false,
                });
                sent_input = true;
            }
        }
        // 兜底：egui 认为所有键都松开了，但远端还按着（抬起事件被别的窗口吃掉）。
        if !ui.input(|i| i.pointer.any_down()) && self.input.buttons.iter().any(|b| *b) {
            for index in 0..3 {
                if self.input.buttons[index] {
                    self.input.buttons[index] = false;
                    controller.send(ClientMsg::MouseButton {
                        button: index as u8,
                        down: false,
                    });
                }
            }
        }

        // ---- 滚轮：按「行」为单位发 120 的整数倍，别送平滑后的碎步 ----
        if over {
            let mut notches = 0i32;
            let mut points = self.input.wheel_accum;
            for event in &events {
                let egui::Event::MouseWheel { unit, delta, .. } = event else {
                    continue;
                };
                match unit {
                    egui::MouseWheelUnit::Line => notches += delta.y.round() as i32,
                    egui::MouseWheelUnit::Page => notches += (delta.y * 3.0).round() as i32,
                    egui::MouseWheelUnit::Point => points += delta.y,
                }
            }
            while points.abs() >= 50.0 {
                let step = 50.0 * points.signum();
                points -= step;
                notches += step.signum() as i32;
            }
            self.input.wheel_accum = points;
            if notches != 0 {
                controller.send(ClientMsg::Wheel {
                    delta: (notches * 120).clamp(-2400, 2400),
                });
                sent_input = true;
            }
        } else {
            self.input.wheel_accum = 0.0;
        }

        // ---- 键盘 ----
        //
        // 上一版要求「画面控件必须有 egui 焦点」，于是点一下工具栏的滑块/下拉/勾选框，
        // 键盘就静默失效，直到用户再点一次画面 —— 这是「有时候键盘不好使」的另一半原因。
        // 现在改成：只要键鼠处于启用状态、窗口有焦点、而且不是在 IP 输入框里打字，就转发。
        let keyboard_active = self.input_armed && focus_ok && !self.target_editing;
        if keyboard_active {
            // Ctrl+V 之前把本地剪贴板强制推一份过去，粘贴就不会拿到旧内容。
            if wants_paste(&events) {
                // 先强制重读本地剪贴板再推过去：Ctrl+V 时被控端拿到的必须是最新内容。
                self.clipboard.invalidate();
                if let Some(payload) = self.clipboard.poll() {
                    controller.send_clip(payload);
                }
            }
            if send_keyboard(ui, controller, &mut self.input, swallow_escape) {
                sent_input = true;
            }
        } else if self.input.held() {
            self.release_remote_input();
        }

        if sent_input {
            self.health.mark_local_input();
        }
    }

    fn escape_hint_active(&self) -> bool {
        self.escape_hint_until
            .map(|until| Instant::now() <= until)
            .unwrap_or(false)
    }

    fn welcome_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("连接到另一台电脑")
                .size(24.0)
                .strong()
                .color(theme::TEXT),
        );
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("两台电脑连同一个 WiFi / 路由器；被控端运行 lanlink.exe --agent")
                .size(15.0)
                .color(theme::TEXT_DIM),
        );
        ui.add_space(10.0);

        theme::card_frame().show(ui, |ui| {
            ui.label(
                egui::RichText::new("目标 IP")
                    .strong()
                    .color(theme::ACCENT)
                    .size(15.0),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.target)
                        .hint_text("例如 192.168.1.23")
                        .desired_width(220.0),
                );
                self.target_editing = resp.has_focus();
                if ui
                    .add(egui::Button::new(egui::RichText::new("连接").strong()))
                    .clicked()
                {
                    self.connect();
                }
                if ui
                    .add_enabled(
                        !self.scanning,
                        egui::Button::new(if self.scanning {
                            "扫描中..."
                        } else {
                            "扫描局域网"
                        }),
                    )
                    .clicked()
                {
                    self.start_scan();
                }
                if self.scanning {
                    ui.spinner();
                }
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("如果被控端开着 VPN，请填它显示的「局域网」IP，不要填 VPN 分配的内网地址。")
                    .size(13.5)
                    .color(theme::TEXT_DIM),
            );
        });

        if !self.peers.is_empty() {
            ui.add_space(8.0);
            theme::card_frame().show(ui, |ui| {
                ui.label(
                    egui::RichText::new("发现的设备（点击连接）")
                        .strong()
                        .color(theme::ACCENT),
                );
                let peers = self.peers.clone();
                for peer in peers {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&peer.name).monospace());
                        ui.label(
                            egui::RichText::new(peer.addr.to_string())
                                .monospace()
                                .color(theme::TEXT_DIM),
                        );
                        if peer.version != crate::protocol::APP_VERSION {
                            ui.label(
                                egui::RichText::new("版本不同").color(theme::WARN).strong(),
                            );
                        }
                        if ui.button("连接").clicked() {
                            self.target = peer.addr.to_string();
                            self.connect();
                        }
                    });
                }
            });
        }

        ui.add_space(8.0);
        theme::card_frame().show(ui, |ui| {
            ui.label(
                egui::RichText::new("本机局域网地址")
                    .strong()
                    .color(theme::ACCENT),
            );
            let ips = net::local_ipv4();
            if ips.is_empty() {
                ui.label(
                    egui::RichText::new("没有找到局域网 IPv4 地址")
                        .color(theme::WARN),
                );
            } else {
                for ip in ips {
                    ui.label(
                        egui::RichText::new(format!("{ip}:{}", DEFAULT_PORT))
                            .monospace()
                            .size(15.0),
                    );
                }
            }
        });
    }

    fn agent_ui(&mut self, ui: &mut egui::Ui) {
        let (port, clients, last_client) = match self.agent.as_ref() {
            Some(agent) => (agent.port, agent.client_count(), agent.last_client()),
            None => (DEFAULT_PORT, 0, String::new()),
        };
        let ips = net::local_ipv4();

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("被控端运行中")
                    .size(24.0)
                    .strong(),
            );
            theme::pill(ui, theme::OK, &format!("已连接控制端 {clients}"), theme::CARD);
        });
        ui.add_space(6.0);

        theme::card_frame().show(ui, |ui| {
            theme::labeled(
                ui,
                "监听端口",
                format!(
                    "TCP {port}（UDP {} 用于局域网发现）",
                    crate::protocol::DISCOVERY_PORT
                ),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("把下面的地址告诉控制端：")
                    .color(theme::TEXT_DIM),
            );
            if ips.is_empty() {
                ui.label(
                    egui::RichText::new("没有找到局域网 IPv4 地址").color(theme::WARN),
                );
            } else {
                for ip in &ips {
                    ui.label(
                        egui::RichText::new(format!("{ip}:{port}"))
                            .monospace()
                            .size(16.0)
                            .color(theme::ACCENT),
                    );
                }
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("接收文件保存到：{}", net::download_dir().display()))
                    .color(theme::TEXT_DIM),
            );
            if !last_client.is_empty() {
                ui.label(
                    egui::RichText::new(format!("最近一次控制端：{last_client}"))
                        .color(theme::TEXT_DIM),
                );
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let mut clip = self.clipboard_enabled;
                if ui.checkbox(&mut clip, "允许剪贴板同步").changed() {
                    self.clipboard_enabled = clip;
                    self.clipboard.set_enabled(clip);
                }
                let mut autostart = self.autostart;
                if ui.checkbox(&mut autostart, "开机自动启动").changed() {
                    match net::set_autostart(autostart) {
                        Ok(()) => {
                            self.autostart = autostart;
                            self.log(if autostart {
                                "已设置开机自启"
                            } else {
                                "已取消开机自启"
                            });
                        }
                        Err(e) => self.log(format!("设置开机自启失败: {e:#}")),
                    }
                }
                if ui.button("放行防火墙（需 UAC）").clicked() {
                    match net::request_firewall_rules() {
                        Ok(()) => {
                            self.firewall_note =
                                "已请求管理员权限，请在 UAC 弹窗中点击「是」。".to_string();
                            self.log(self.firewall_note.clone());
                        }
                        Err(e) => {
                            self.firewall_note = format!("请求失败: {e:#}");
                            self.log(self.firewall_note.clone());
                        }
                    }
                }
                if ui.button("复制局域网地址").clicked() {
                    let ip = ips
                        .iter()
                        .find(|ip| ip.starts_with("192.168."))
                        .or_else(|| ips.first());
                    if let Some(ip) = ip {
                        ui.ctx().copy_text(format!("{ip}:{port}"));
                        self.status = format!("已复制 {ip}:{port}");
                    }
                }
            });
            if !self.firewall_note.is_empty() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&self.firewall_note).color(theme::TEXT_DIM));
            }
        });

        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "提示：控制端切到「只看画面」后，本机会拒绝一切注入的键鼠操作；\
                 如果键鼠突然失灵，在控制端按 F8 或点「修复键鼠」即可。",
            )
            .size(13.5)
            .color(theme::TEXT_DIM),
        );

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("日志").size(18.0).strong());
            if ui.button("清空").clicked() {
                if let Ok(mut logs) = self.logs.lock() {
                    logs.clear();
                }
            }
        });
        let lines = self.log_lines();
        egui::Frame::default()
            .fill(theme::CARD)
            .stroke(egui::Stroke::new(1.0, theme::LINE))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in lines {
                            ui.label(
                                egui::RichText::new(line)
                                    .monospace()
                                    .size(13.5)
                                    .color(theme::TEXT),
                            );
                        }
                    });
            });
    }

    fn log_lines(&self) -> Vec<String> {
        self.logs
            .lock()
            .map(|logs| {
                logs.iter()
                    .rev()
                    .take(500)
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let connected = self
            .controller
            .as_ref()
            .map(|c| c.is_connected())
            .unwrap_or(false);
        ui.horizontal(|ui| {
            let color = if connected {
                theme::OK
            } else if self.mode == Mode::Agent && self.agent.is_some() {
                theme::ACCENT
            } else if self.controller.is_some() {
                theme::WARN
            } else {
                theme::TEXT_DIM
            };
            ui.colored_label(color, "●");
            let status = self.status.clone();
            ui.label(egui::RichText::new(status).color(theme::TEXT));
            if connected {
                ui.separator();
                ui.label(
                    egui::RichText::new(format!(
                        "{:.0} fps · {:.1} MB/s · 延迟 {} ms · 解码 {:.1} ms",
                        self.stats.fps, self.stats.mbps, self.stats.rtt_ms, self.stats.decode_ms
                    ))
                    .color(theme::TEXT_DIM),
                );
                // 把「被控端到底收到并注入成功了多少」直接摊在状态栏上：
                // 出问题时能立刻分清是「消息没过去」还是「系统不收」。
                if let Some(status) = self.health.status.as_ref() {
                    let (text, color) = if status.gated {
                        ("键鼠 已暂停".to_string(), theme::WARN)
                    } else if status.failed > 0 && !status.ok {
                        (
                            format!("键鼠 注入 {} 失败 {}", status.injected, status.failed),
                            theme::ERR,
                        )
                    } else {
                        (
                            format!("键鼠 注入 {}", status.injected),
                            theme::TEXT_DIM,
                        )
                    };
                    ui.label(egui::RichText::new(text).color(color));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(controller) = self.controller.as_ref() {
                    ui.label(
                        egui::RichText::new(controller.addr.to_string())
                            .color(theme::TEXT_DIM),
                    );
                }
                if self.mode == Mode::Controller {
                    ui.label(
                        egui::RichText::new(if self.control_enabled {
                            "可操作"
                        } else {
                            "只看画面"
                        })
                        .color(if self.control_enabled {
                            theme::TEXT_DIM
                        } else {
                            theme::WARN
                        }),
                    );
                }
                ui.label(
                    egui::RichText::new(if self.clipboard_enabled {
                        "剪贴板 开"
                    } else {
                        "剪贴板 关"
                    })
                    .color(theme::TEXT_DIM),
                );
            });
        });
    }

    fn draw_toast(&mut self, ctx: &egui::Context) {
        let Some((text, color, until)) = self.toast.clone() else {
            return;
        };
        if Instant::now() > until {
            self.toast = None;
            return;
        }
        egui::Area::new(egui::Id::new("lanlink-toast"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-18.0, -52.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(egui::Color32::from_rgb(24, 27, 35))
                    .stroke(egui::Stroke::new(1.0, color))
                    .corner_radius(egui::CornerRadius::same(9))
                    .inner_margin(egui::Margin::symmetric(14, 9))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&text).color(theme::TEXT).size(14.5));
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(120));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 系统主题一旦被切回浅色，这里会自愈回来（上一版就是被系统浅色主题带跑的）。
        if theme::needs_reapply(ctx) {
            theme::apply(ctx);
        }
        if self.ctx.is_none() {
            self.ctx = Some(ctx.clone());
        }
        let controller_mode = self.mode == Mode::Controller;
        if ctx.input(|i| i.key_pressed(egui::Key::F11)) && controller_mode {
            self.toggle_fullscreen(ctx);
        }
        if controller_mode {
            if ctx.input(|i| i.key_pressed(egui::Key::F8)) && self.controller.is_some() {
                self.repair_input(true);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::F9)) && self.controller.is_some() {
                if let Some(controller) = self.controller.as_ref() {
                    controller.reconnect_now();
                }
                self.input.reset();
                self.health.reset();
                self.log("手动重连（F9）");
            }
        }
        self.poll_controller_events(ctx);
        self.poll_scan();
        self.refresh_stats();
        self.send_quality_if_needed();
        self.poll_input_health();

        if self.exit_requested {
            self.exit_requested = false;
            self.last_escape = None;
            self.escape_hint_until = None;
            if self.fullscreen {
                self.toggle_fullscreen(ctx);
            }
            self.disconnect();
            self.status = "已退出远程控制（双击 Esc）".to_string();
            self.log(self.status.clone());
        }

        // 画面到达会通过 FrameBoard 的 wake 回调直接触发重绘，这里的定时重绘只负责兜底和提示计时。
        if self
            .controller
            .as_ref()
            .map(|c| c.is_connected())
            .unwrap_or(false)
        {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if self.controller.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.scanning {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.mode == Mode::Agent {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        egui::TopBottomPanel::top("top")
            .frame(theme::bar_frame())
            .show(ctx, |ui| self.top_bar(ui, ctx));
        egui::TopBottomPanel::bottom("bottom")
            .frame(theme::bar_frame())
            .show(ctx, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(theme::central_frame())
            .show(ctx, |ui| match self.mode {
                Mode::Agent => self.agent_ui(ui),
                Mode::Controller => self.controller_ui(ui),
            });
        self.draw_toast(ctx);
    }

    /// 显式给窗口一个不透明底色。
    ///
    /// eframe 默认返回半透明的 12,12,12，任何「没画背景」的区域都会露出这个近黑色 ——
    /// 上一版顶栏/底栏的文字看不清，正是这两件事叠加的结果。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }
}

/// 整块画布的 RGBA 字节直接当成 Color32 序列（Color32 是 repr(C) 的 [u8; 4]）。
fn color_image_full(canvas: &Canvas) -> egui::ColorImage {
    const _: () = assert!(std::mem::size_of::<egui::Color32>() == 4);
    let size = [canvas.grid.width as usize, canvas.grid.height as usize];
    let count = canvas.rgba.len() / 4;
    let mut pixels: Vec<egui::Color32> = Vec::with_capacity(count);
    unsafe {
        std::ptr::copy_nonoverlapping(
            canvas.rgba.as_ptr(),
            pixels.as_mut_ptr().cast::<u8>(),
            count * 4,
        );
        pixels.set_len(count);
    }
    egui::ColorImage { size, pixels }
}

/// 只拷贝画布上的一个矩形（脏瓦片）。
fn color_image_rect(canvas: &Canvas, x: u32, y: u32, w: u32, h: u32) -> egui::ColorImage {
    let stride = canvas.grid.width as usize * 4;
    let row_bytes = w as usize * 4;
    let count = row_bytes * h as usize / 4;
    let mut pixels: Vec<egui::Color32> = Vec::with_capacity(count);
    for row in 0..h as usize {
        let start = (y as usize + row) * stride + x as usize * 4;
        let end = start + row_bytes;
        if end > canvas.rgba.len() {
            break;
        }
        unsafe {
            let dst = pixels.as_mut_ptr().cast::<u8>().add(row * row_bytes);
            std::ptr::copy_nonoverlapping(canvas.rgba.as_ptr().add(start), dst, row_bytes);
        }
    }
    unsafe { pixels.set_len(count) }
    egui::ColorImage {
        size: [w as usize, h as usize],
        pixels,
    }
}

fn draw_badge(ui: &egui::Ui, anchor: egui::Pos2, text: &str, color: egui::Color32) {
    let galley = ui.painter().layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(15.0),
        color,
    );
    let center = anchor + galley.size() / 2.0;
    let bg = egui::Rect::from_center_size(center, galley.size() + egui::vec2(24.0, 12.0));
    ui.painter().rect_filled(
        bg,
        egui::CornerRadius::same(8),
        egui::Color32::from_black_alpha(210),
    );
    ui.painter()
        .galley(bg.center() - galley.size() / 2.0, galley, color);
}

fn normalize(p: egui::Pos2, rect: egui::Rect) -> (f32, f32) {
    let nx = ((p.x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0);
    let ny = ((p.y - rect.min.y) / rect.height().max(1.0)).clamp(0.0, 1.0);
    (nx, ny)
}

fn button_index(button: egui::PointerButton) -> Option<usize> {
    match button {
        egui::PointerButton::Primary => Some(0),
        egui::PointerButton::Secondary => Some(1),
        egui::PointerButton::Middle => Some(2),
        _ => None,
    }
}

/// 这一帧里有没有「粘贴」意图。
///
/// egui-winit 会把 Ctrl+V / Shift+Insert 直接翻译成 `Event::Paste`，
/// 根本不会产生 `Event::Key`，所以必须认这个变体（以前只认 Key::V，永远命不中）。
fn wants_paste(events: &[egui::Event]) -> bool {
    events.iter().any(|e| match e {
        egui::Event::Paste(_) => true,
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            (matches!(key, egui::Key::V) && modifiers.ctrl)
                || (matches!(key, egui::Key::Insert) && modifiers.shift)
        }
        _ => false,
    })
}

fn push_log(logs: &Arc<Mutex<Vec<String>>>, msg: String) {
    if let Ok(mut logs) = logs.lock() {
        logs.push(format!(
            "[{}] {}",
            chrono::Local::now().format("%H:%M:%S"),
            msg
        ));
        if logs.len() > 1500 {
            let drop_count = logs.len() - 1500;
            logs.drain(0..drop_count);
        }
    }
}

/// 把修饰键同步到「此刻远端应该处于的状态」。
///
/// 关键点：必须按事件顺序、在真正的按键之前发出 Ctrl/Shift/Alt，
/// 否则同一帧里「Ctrl 按下 + C 按下」会被压成裸 C —— 这就是 Ctrl+C 偶尔失效的原因。
///
/// 发的是左修饰键的真实虚拟码（0xA2 Ctrl / 0xA0 Shift / 0xA4 Alt），
/// 因为物理键盘发的就是这几个码：只发通用的 0x11/0x10/0x12 时，
/// 一部分应用（读扫描码或 GetAsyncKeyState(VK_LCONTROL) 的）会当作没有按下。
fn sync_mods(controller: &ControllerHandle, state: &mut RemoteInput, mods: egui::Modifiers) {
    let wanted = [mods.ctrl, mods.shift, mods.alt];
    for (index, vk) in [0xA2u16, 0xA0, 0xA4].into_iter().enumerate() {
        if wanted[index] != state.mods[index] {
            if controller.send(ClientMsg::Key {
                vk,
                down: wanted[index],
            }) {
                state.mods[index] = wanted[index];
            } else {
                return;
            }
        }
    }
}

/// 把「复制 / 剪切 / 粘贴」还原成真正的快捷键发给被控端。
///
/// egui-winit 在键盘事件进来时就把 Ctrl+C / Ctrl+X / Ctrl+V（Windows 上还有
/// Ctrl+Insert / Shift+Insert / Shift+Delete）翻译成 `Event::Copy` / `Cut` /
/// `Paste` 并直接 `return`，**不会**再产生 `Event::Key`。所以只处理 Key 事件的话，
/// 复制粘贴永远发不出去 —— 这正是「Ctrl+C/V 在被控端没反应」的原因。
fn inject_combo(controller: &ControllerHandle, state: &mut RemoteInput, vk: u16) {
    // 这三个命令在语义上都带 Ctrl。物理 Ctrl 可能已经被前面的 Key 事件同步下去，
    // 也可能同一帧里按下就松开（此时整帧的 modifiers 已经是松开状态），
    // 所以这里主动补一个 Ctrl 按下；抬起交给帧尾的 sync_mods(final_mods)。
    if !state.mods[0] {
        if controller.send(ClientMsg::Key { vk: 0xA2, down: true }) {
            state.mods[0] = true;
        } else {
            return;
        }
    }
    if controller.send(ClientMsg::Key { vk, down: true }) {
        state.press_key(vk);
    }
    if state.release_key(vk) {
        controller.send(ClientMsg::Key { vk, down: false });
    }
}

/// 转发这一帧的键盘事件，返回「有没有真的发出去东西」。
fn send_keyboard(
    ui: &egui::Ui,
    controller: &ControllerHandle,
    state: &mut RemoteInput,
    skip_escape: bool,
) -> bool {
    let (events, final_mods) = ui.input(|i| (i.events.clone(), i.modifiers));
    let mut sent = false;
    for event in &events {
        match event {
            // 复制 / 剪切 / 粘贴被 egui 吞成了独立事件，必须在这里翻译回快捷键。
            egui::Event::Copy => {
                inject_combo(controller, state, 0x43);
                sent = true;
            } // C
            egui::Event::Cut => {
                inject_combo(controller, state, 0x58);
                sent = true;
            } // X
            egui::Event::Paste(_) => {
                inject_combo(controller, state, 0x56);
                sent = true;
            } // V
            egui::Event::Key {
                key,
                pressed,
                repeat,
                modifiers,
                ..
            } => {
                // F8 / F9 / F11 是本地快捷键，不转发给被控端。
                if matches!(*key, egui::Key::F8 | egui::Key::F9 | egui::Key::F11) {
                    continue;
                }
                if skip_escape && *key == egui::Key::Escape {
                    continue;
                }
                let Some(vk) = vk_for_key(*key) else {
                    continue;
                };
                if *pressed {
                    // 修饰键先落位，再发按键本身。
                    sync_mods(controller, state, *modifiers);
                    if *repeat {
                        continue;
                    }
                    // Shift+数字交给 Text（符号）走 Unicode 注入，避免打出两个字符。
                    if modifiers.shift && is_digit_key(*key) {
                        continue;
                    }
                    if controller.send(ClientMsg::Key { vk, down: true }) {
                        state.press_key(vk);
                        sent = true;
                    }
                } else {
                    // 抬起顺序与按下相反：先键、后修饰键。
                    if state.release_key(vk) {
                        controller.send(ClientMsg::Key { vk, down: false });
                        sent = true;
                    }
                    sync_mods(controller, state, *modifiers);
                }
            }
            egui::Event::Text(text) => {
                if text.is_empty() {
                    continue;
                }
                let plain = text.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ');
                if !plain {
                    controller.send(ClientMsg::Text { text: text.clone() });
                    sent = true;
                }
            }
            _ => {}
        }
    }
    sync_mods(controller, state, final_mods);
    sent
}

fn is_digit_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::Num0
            | egui::Key::Num1
            | egui::Key::Num2
            | egui::Key::Num3
            | egui::Key::Num4
            | egui::Key::Num5
            | egui::Key::Num6
            | egui::Key::Num7
            | egui::Key::Num8
            | egui::Key::Num9
    )
}

fn vk_for_key(key: egui::Key) -> Option<u16> {
    use egui::Key as K;
    Some(match key {
        K::ArrowDown => 0x28,
        K::ArrowLeft => 0x25,
        K::ArrowRight => 0x27,
        K::ArrowUp => 0x26,
        K::Escape => 0x1B,
        K::Tab => 0x09,
        K::Backspace => 0x08,
        K::Enter => 0x0D,
        K::Space => 0x20,
        K::Insert => 0x2D,
        K::Delete => 0x2E,
        K::Home => 0x24,
        K::End => 0x23,
        K::PageUp => 0x21,
        K::PageDown => 0x22,
        K::A => 0x41,
        K::B => 0x42,
        K::C => 0x43,
        K::D => 0x44,
        K::E => 0x45,
        K::F => 0x46,
        K::G => 0x47,
        K::H => 0x48,
        K::I => 0x49,
        K::J => 0x4A,
        K::K => 0x4B,
        K::L => 0x4C,
        K::M => 0x4D,
        K::N => 0x4E,
        K::O => 0x4F,
        K::P => 0x50,
        K::Q => 0x51,
        K::R => 0x52,
        K::S => 0x53,
        K::T => 0x54,
        K::U => 0x55,
        K::V => 0x56,
        K::W => 0x57,
        K::X => 0x58,
        K::Y => 0x59,
        K::Z => 0x5A,
        K::Num0 => 0x30,
        K::Num1 => 0x31,
        K::Num2 => 0x32,
        K::Num3 => 0x33,
        K::Num4 => 0x34,
        K::Num5 => 0x35,
        K::Num6 => 0x36,
        K::Num7 => 0x37,
        K::Num8 => 0x38,
        K::Num9 => 0x39,
        K::F1 => 0x70,
        K::F2 => 0x71,
        K::F3 => 0x72,
        K::F4 => 0x73,
        K::F5 => 0x74,
        K::F6 => 0x75,
        K::F7 => 0x76,
        K::F8 => 0x77,
        K::F9 => 0x78,
        K::F10 => 0x79,
        K::F11 => 0x7A,
        K::F12 => 0x7B,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ago(d: Duration) -> Instant {
        Instant::now() - d
    }

    /// 正常通道：被控端的确认比本地输入还新（每次探测都会刷新），不应判故障。
    #[test]
    fn healthy_channel_is_not_stalled() {
        let health = InputHealth {
            last_change: Some(ago(Duration::from_secs(5))),
            last_local_input: Some(ago(Duration::from_secs(6))),
            ..Default::default()
        };
        assert!(!health.stalled());
    }

    /// 完全挂机：从来没发过键鼠，哪怕确认早就不动了也不能报警。
    #[test]
    fn idle_without_input_is_not_stalled() {
        let health = InputHealth {
            last_change: Some(ago(Duration::from_secs(30))),
            last_local_input: None,
            ..Default::default()
        };
        assert!(!health.stalled());
    }

    /// 刚发完指令、确认还没回来（探测间隔 500ms）：不能马上报警。
    #[test]
    fn fresh_input_without_ack_is_not_stalled_yet() {
        let health = InputHealth {
            last_change: Some(ago(Duration::from_millis(200))),
            last_local_input: Some(ago(Duration::from_millis(100))),
            ..Default::default()
        };
        assert!(!health.stalled());
    }

    /// 通道堵死：用户发了指令，但被控端的计数 1.5 秒没动过 —— 这才是真卡住。
    #[test]
    fn blocked_channel_is_stalled() {
        let health = InputHealth {
            last_change: Some(ago(Duration::from_secs(2))),
            last_local_input: Some(ago(Duration::from_millis(900))),
            ..Default::default()
        };
        assert!(health.stalled());
    }

    /// 偶发一次 SendInput 失败不弹红条，连续两次才认。
    #[test]
    fn single_injection_failure_is_ignored() {
        let mut health = InputHealth::default();
        health.bad_streak = 1;
        assert!(health.bad_streak < 2);
    }

    /// 会话号变了（旧连接被顶掉 / 刚重连）：计数清零，不能拿旧值当参考。
    #[test]
    fn session_change_resets_counters() {
        let mut health = InputHealth::default();
        health.on_status(InputStatusView {
            session: 1,
            received: 40,
            injected: 40,
            failed: 0,
            gated: false,
            ok: true,
        });
        assert_eq!(health.last_received, 40);
        health.repairs = 2;
        health.hard_reconnects = 3;
        health.gave_up = true;
        health.on_status(InputStatusView {
            session: 2,
            received: 0,
            injected: 0,
            failed: 0,
            gated: false,
            ok: true,
        });
        assert_eq!(health.last_received, 0);
        assert_eq!(health.repairs, 0);
        assert_eq!(health.hard_reconnects, 0);
        assert!(!health.gave_up);
    }
}
