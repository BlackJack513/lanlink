use crate::capture::virtual_screen;
use crate::clipboard::{ClipboardPayload, SharedClipboard};
use crate::frame::{Canvas, FrameMessage, TileGrid};
use crate::input;
use crate::pipeline::{self, Quality};
use crate::protocol::*;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub type LogFn = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Peer {
    pub name: String,
    pub addr: SocketAddr,
    pub version: u32,
}

#[derive(Clone, Debug)]
pub enum NetEvent {
    Connected(ServerHello),
    Disconnected(String),
    Log(String),
    Clipboard(ClipboardPayload),
    FileSaved(String, u64),
    Notice(String),
    /// 被控端的键鼠通道健康报告（每 500 ms 一次）。
    InputStatus(InputStatusView),
}

/// 被控端上报的键鼠通道状态。
#[derive(Clone, Debug)]
pub struct InputStatusView {
    pub session: u64,
    /// 被控端累计收到的键鼠消息数。
    pub received: u64,
    /// 成功注入系统的事件数。
    pub injected: u64,
    /// SendInput 被拒绝的次数。
    pub failed: u64,
    /// 被控端是否处于「只看画面」。
    pub gated: bool,
    /// 最近一次注入是否被系统接受。
    pub ok: bool,
}

/// 被控端当前会话编号 + 会话占用情况。
///
/// 以前用 `busy` 标志「有控制端就拒绝新连接」，问题是：旧会话可能卡在读写上十几秒
/// 才退出，这段时间里控制端**连回来也会被拒**，用户看到的就是「重连也连不上/连上也动不了」。
/// 现在改成「新会话顶掉旧会话」：立刻 shutdown 旧 socket，保证重连永远是干净的。
type Seat = Mutex<Option<(u64, TcpStream)>>;

pub struct AgentHandle {
    pub port: u16,
    pub clients: Arc<AtomicU64>,
    pub last_client: Arc<Mutex<String>>,
    pub stop: Arc<AtomicBool>,
}

impl AgentHandle {
    pub fn client_count(&self) -> u64 {
        self.clients.load(Ordering::Relaxed)
    }
    pub fn last_client(&self) -> String {
        self.last_client
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn host_name() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "Windows".to_string())
}

pub fn local_ipv4() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let if_addrs::IfAddr::V4(v4) = &iface.addr {
                if !v4.ip.is_loopback() && !v4.ip.is_link_local() {
                    out.push(v4.ip.to_string());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn download_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(custom) = std::env::var("LANLINK_DOWNLOAD_DIR") {
            let dir = PathBuf::from(custom);
            if std::fs::create_dir_all(&dir).is_ok() {
                let probe = dir.join(".lanlink_write_test");
                if std::fs::File::create(&probe).is_ok() {
                    let _ = std::fs::remove_file(&probe);
                    return dir;
                }
            }
        }
        let mut candidates = Vec::new();
        if let Some(base) = dirs::download_dir() {
            candidates.push(base.join("LanLink"));
        }
        if let Some(base) = dirs::home_dir() {
            candidates.push(base.join("LanLinkDownloads"));
        }
        if let Some(base) = dirs::data_local_dir() {
            candidates.push(base.join("LanLink").join("Downloads"));
        }
        for dir in candidates {
            if std::fs::create_dir_all(&dir).is_err() {
                continue;
            }
            let probe = dir.join(".lanlink_write_test");
            if std::fs::File::create(&probe).is_ok() {
                let _ = std::fs::remove_file(&probe);
                return dir;
            }
        }
        dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("LanLink")
            .join("Downloads")
    })
    .clone()
}

pub fn start_agent(
    port: u16,
    clipboard: SharedClipboard,
    log: LogFn,
    discovery: bool,
) -> Result<AgentHandle> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .with_context(|| format!("监听 TCP {port} 失败（端口被占用？）"))?;
    listener.set_nonblocking(true).ok();
    let stop = Arc::new(AtomicBool::new(false));
    let clients = Arc::new(AtomicU64::new(0));
    let last_client = Arc::new(Mutex::new(String::new()));
    let session = Arc::new(AtomicU64::new(0));
    let seat: Arc<Seat> = Arc::new(Mutex::new(None));

    {
        let stop = stop.clone();
        let clients = clients.clone();
        let last_client = last_client.clone();
        let session = session.clone();
        let seat = seat.clone();
        let log = log.clone();
        let clipboard = clipboard.clone();
        thread::spawn(move || {
            log(format!("被控端已启动，监听 0.0.0.0:{port}"));
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let clients = clients.clone();
                        let last_client = last_client.clone();
                        let session = session.clone();
                        let seat = seat.clone();
                        let log = log.clone();
                        let clipboard = clipboard.clone();
                        thread::spawn(move || {
                            let _ = stream.set_nodelay(true);
                            if let Err(e) = handle_incoming(
                                stream,
                                addr,
                                clipboard,
                                log.clone(),
                                clients,
                                last_client,
                                session,
                                seat,
                            ) {
                                log(format!("连接 {addr} 结束: {e:#}"));
                            }
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => {
                        log(format!("accept 失败: {e}"));
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        });
    }

    if discovery {
        let log = log.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            if let Err(e) = discovery_server(port, log.clone(), stop) {
                log(format!("局域网发现服务启动失败: {e:#}"));
            }
        });
    }

    Ok(AgentHandle {
        port,
        clients,
        last_client,
        stop,
    })
}

fn discovery_server(port: u16, log: LogFn, stop: Arc<AtomicBool>) -> Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT))
        .with_context(|| format!("绑定 UDP {DISCOVERY_PORT} 失败"))?;
    socket.set_broadcast(true).ok();
    socket
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok();
    let reply = DiscoveryReply {
        magic: String::from_utf8_lossy(DISCOVERY_MAGIC).to_string(),
        name: host_name(),
        port,
        version: APP_VERSION,
    };
    let payload = serde_json::to_vec(&reply)?;
    log(format!("局域网发现已启动 UDP {DISCOVERY_PORT}"));
    let mut buf = [0u8; 512];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                if &buf[..len] == DISCOVERY_MAGIC {
                    let _ = socket.send_to(&payload, src);
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => log(format!("发现服务 recv 失败: {e}")),
        }
    }
    Ok(())
}

/// 会话占位：离开时把自己从「座位」上摘掉（但不要误删后来者的座位）。
struct SeatGuard {
    seat: Arc<Seat>,
    session: u64,
}
impl Drop for SeatGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.seat.lock() {
            if matches!(guard.as_ref(), Some((id, _)) if *id == self.session) {
                *guard = None;
            }
        }
    }
}

fn handle_incoming(
    mut stream: TcpStream,
    addr: SocketAddr,
    clipboard: SharedClipboard,
    log: LogFn,
    clients: Arc<AtomicU64>,
    last_client: Arc<Mutex<String>>,
    session_counter: Arc<AtomicU64>,
    seat: Arc<Seat>,
) -> Result<()> {
    stream
        .set_nonblocking(false)
        .context("设置控制连接为阻塞模式")?;
    // 小包立刻发出，避免 Nagle 给鼠标/键盘输入叠加 40 ms 延迟。
    let _ = stream.set_nodelay(true);
    // 对端没了的时候 write_all 不能无限期挂着，否则采集线程会跟着卡死。
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let (kind, payload) = read_frame(&mut stream).context("读取首帧")?;
    if kind == KIND_FILE {
        return receive_file(stream, &payload, log);
    }
    if kind != KIND_CONTROL {
        bail!("未知首帧类型 {kind}");
    }
    let hello: ControlHello = parse_json(&payload).context("解析控制端握手")?;
    if hello.version != APP_VERSION {
        let _ = write_json(
            &mut stream,
            KIND_CONTROL,
            &ServerMsg::Notice {
                text: format!("版本不匹配：控制端 {} / 被控端 {}", hello.version, APP_VERSION),
            },
        );
        bail!("版本不匹配");
    }
    // 不拒绝新连接：直接把旧会话顶掉。
    //
    // 旧会话的主循环可能正卡在 read/write timeout 里（最长十几秒），以前这段时间
    // 新连接会被「已有控制端连接」挡回去，用户看到的就是「重连也没用」。
    let session = session_counter.fetch_add(1, Ordering::SeqCst) + 1;
    match stream.try_clone() {
        Ok(handle) => {
            let previous = seat.lock().ok().and_then(|mut s| s.replace((session, handle)));
            if let Some((old_session, old_stream)) = previous {
                log(format!("新控制端 {addr} 接入，结束旧会话 #{old_session}"));
                // 立刻打断旧会话的读写，让它在一个循环内退出。
                let _ = old_stream.shutdown(Shutdown::Both);
                // 旧会话可能给被控端留下了「按着不放」的键鼠。
                input::release_all();
            }
        }
        Err(e) => log(format!("复制会话句柄失败: {e}")),
    }
    let _seat = SeatGuard {
        seat: seat.clone(),
        session,
    };
    clients.fetch_add(1, Ordering::SeqCst);
    let _client_guard = ClientGuard(clients.clone());
    *last_client.lock().unwrap() = format!("{} ({})", hello.host, addr);

    let rect = virtual_screen()?;
    let server_hello = ServerHello {
        version: APP_VERSION,
        host: host_name(),
        width: rect.width,
        height: rect.height,
        clipboard_enabled: clipboard.enabled.load(Ordering::Relaxed),
    };
    write_json(&mut stream, KIND_CONTROL, &server_hello)?;
    log(format!(
        "控制端 {} ({}) 已连接，屏幕 {}x{}",
        hello.host, addr, rect.width, rect.height
    ));

    let reader = stream.try_clone().context("复制连接句柄")?;
    // 8 秒收不到任何东西（控制端每 3 秒必然 Ping 一次）就认为控制端已经走了。
    // 这样半死的 TCP 连接最多占座 8 秒就会被自己清掉，而不是原来的 15 秒。
    reader
        .set_read_timeout(Some(Duration::from_secs(8)))
        .context("设置控制通道超时")?;
    let writer = Arc::new(Mutex::new(stream));
    let alive = Arc::new(AtomicBool::new(true));
    let quality = Arc::new(Mutex::new(Quality::default()));

    pipeline::start_agent_pipeline(writer.clone(), alive.clone(), quality.clone(), log.clone());
    // 握手后立刻上报一次键鼠通道状态，控制端就有了基准值。
    send_input_status(&writer, &alive, session, true);
    {
        let writer = writer.clone();
        let alive = alive.clone();
        let clipboard = clipboard.clone();
        thread::spawn(move || {
            while alive.load(Ordering::Relaxed) {
                if clipboard.enabled.load(Ordering::Relaxed) {
                    if let Some(payload) = clipboard.poll() {
                        if !send_clipboard(&writer, &alive, &payload) {
                            break;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(80));
            }
        });
    }

    control_reader(
        reader,
        writer.clone(),
        alive.clone(),
        quality,
        clipboard,
        log.clone(),
        session,
        session_counter.clone(),
    );
    input::release_all();
    alive.store(false, Ordering::Relaxed);
    if let Ok(stream) = writer.lock() {
        let _ = stream.shutdown(Shutdown::Both);
    }
    log(format!("控制端 {addr} 已断开"));
    Ok(())
}

struct ClientGuard(Arc<AtomicU64>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 本会话收到的键鼠消息总数。控制端拿它和本地发送数对比，就能判断「堵没堵」。
static INPUT_RECEIVED: AtomicU64 = AtomicU64::new(0);

/// 给控制端回一条「键鼠通道状态」。
///
/// 这是自愈机制的地基：控制端每 500 ms 要一次，用它判断指令到底有没有被收到、
/// 到底有没有注入成功。以前这些全是黑盒，用户只能感觉到「突然就不动了」。
fn send_input_status(
    writer: &Arc<Mutex<TcpStream>>,
    alive: &Arc<AtomicBool>,
    session: u64,
    input_enabled: bool,
) -> bool {
    let (injected, failed, ok) = input::health();
    let msg = ServerMsg::InputStatus {
        session,
        received: INPUT_RECEIVED.load(Ordering::Relaxed),
        injected,
        failed,
        gated: !input_enabled,
        ok: ok && input_enabled,
    };
    if !alive.load(Ordering::Relaxed) {
        return false;
    }
    match writer.lock() {
        Ok(mut stream) => write_json(&mut *stream, KIND_CONTROL, &msg).is_ok(),
        Err(_) => false,
    }
}

fn control_reader(
    mut reader: TcpStream,
    writer: Arc<Mutex<TcpStream>>,
    alive: Arc<AtomicBool>,
    quality: Arc<Mutex<Quality>>,
    clipboard: SharedClipboard,
    log: LogFn,
    session: u64,
    epoch: Arc<AtomicU64>,
) {
    let mut input_enabled = true;
    // 被新会话顶掉时 epoch 会变，这里要立刻退出，把键鼠让给新会话。
    while alive.load(Ordering::Relaxed) && epoch.load(Ordering::Relaxed) == session {
        let (kind, payload) = match read_frame(&mut reader) {
            Ok(v) => v,
            Err(e) => {
                if alive.load(Ordering::Relaxed) {
                    log(format!("读取控制消息失败: {e:#}"));
                }
                break;
            }
        };
        match kind {
            KIND_CONTROL => {
                let Ok(msg) = parse_json::<ClientMsg>(&payload) else {
                    continue;
                };
                match msg {
                    ClientMsg::SetInput { enabled } => {
                        if enabled != input_enabled {
                            input_enabled = enabled;
                            if !enabled {
                                input::release_all();
                            }
                            log(if enabled {
                                "控制端已恢复远程操作".to_string()
                            } else {
                                "控制端切换为「只看画面」：本机不再接受注入的键鼠".to_string()
                            });
                        }
                        // 回执给控制端，界面上能明确看到哪一端在「只看画面」。
                        let text = if enabled {
                            "被控端确认：已恢复键鼠注入"
                        } else {
                            "被控端确认：只看画面，已停止键鼠注入"
                        };
                        if let Ok(mut stream) = writer.lock() {
                            let _ = write_json(
                                &mut *stream,
                                KIND_CONTROL,
                                &ServerMsg::Notice {
                                    text: text.to_string(),
                                },
                            );
                        }
                        send_input_status(&writer, &alive, session, input_enabled);
                    }
                    ClientMsg::Ping => {
                        let _ = send_raw(&writer, &alive, KIND_PONG, &[]);
                    }
                    ClientMsg::SetQuality {
                        quality: q,
                        fps,
                        max_width,
                    } => {
                        if let Ok(mut value) = quality.lock() {
                            value.quality = q.clamp(20, 95);
                            value.fps = fps.clamp(1, 60);
                            value.max_width = max_width.clamp(640, 7680);
                        }
                    }
                    ClientMsg::InputProbe => {
                        send_input_status(&writer, &alive, session, input_enabled);
                    }
                    ClientMsg::ResyncInput => {
                        // 控制端说「我这边按键状态乱了」：先把本机所有按下的键弹起，
                        // 再把当前通道状态回过去。这是修复按钮的「软修」。
                        input::release_all();
                        log("控制端请求重同步：已松开本机所有按键".to_string());
                        send_input_status(&writer, &alive, session, input_enabled);
                    }
                    other => {
                        // 「只看画面」时即使收到键鼠消息也直接丢弃，双保险。
                        if input_enabled {
                            INPUT_RECEIVED.fetch_add(1, Ordering::Relaxed);
                            apply_input(other);
                        }
                    }
                }
            }
            KIND_CLIP_TEXT => {
                if clipboard.enabled.load(Ordering::Relaxed) {
                    if let Ok(text) = String::from_utf8(payload) {
                        if let Err(e) = clipboard.apply(&ClipboardPayload::Text(text)) {
                            log(format!("写入剪贴板失败: {e:#}"));
                        }
                    }
                }
            }
            KIND_CLIP_IMAGE => {
                if clipboard.enabled.load(Ordering::Relaxed) {
                    if let Err(e) = clipboard.apply(&ClipboardPayload::Image(payload)) {
                        log(format!("写入图片剪贴板失败: {e:#}"));
                    }
                }
            }
            KIND_PING => {
                let _ = send_raw(&writer, &alive, KIND_PONG, &[]);
            }
            _ => {}
        }
    }
    alive.store(false, Ordering::Relaxed);
}

fn apply_input(msg: ClientMsg) {
    match msg {
        ClientMsg::MouseMove { x, y } => input::mouse_move(x, y),
        ClientMsg::MouseButton { button, down } => input::mouse_button(button, down),
        ClientMsg::Wheel { delta } => input::wheel(delta),
        ClientMsg::Key { vk, down } => {
            input::key(vk, down);
        }
        ClientMsg::Text { text } => {
            input::text(&text);
        }
        ClientMsg::Ping | ClientMsg::SetQuality { .. } | ClientMsg::SetInput { .. } => {}
        ClientMsg::InputProbe | ClientMsg::ResyncInput => {}
    }
}

pub(crate) fn send_raw(
    writer: &Arc<Mutex<TcpStream>>,
    alive: &Arc<AtomicBool>,
    kind: u8,
    data: &[u8],
) -> bool {
    let mut stream = match writer.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    if write_frame(&mut stream, kind, data).is_ok() {
        true
    } else {
        alive.store(false, Ordering::Relaxed);
        let _ = stream.shutdown(Shutdown::Both);
        false
    }
}

fn send_clipboard(
    writer: &Arc<Mutex<TcpStream>>,
    alive: &Arc<AtomicBool>,
    payload: &ClipboardPayload,
) -> bool {
    match payload {
        ClipboardPayload::Text(text) => send_raw(writer, alive, KIND_CLIP_TEXT, text.as_bytes()),
        ClipboardPayload::Image(png) => send_raw(writer, alive, KIND_CLIP_IMAGE, png),
    }
}

fn receive_file(mut stream: TcpStream, hello_payload: &[u8], log: LogFn) -> Result<()> {
    let hello: FileHello = parse_json(hello_payload).context("解析文件信息")?;
    let dir = download_dir();
    log(format!("接收文件到 {}", dir.display()));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建下载目录 {}", dir.display()))?;
    let name = sanitize_name(&hello.name);
    let path = unique_path(&dir, &name);
    let file = File::create(&path).with_context(|| format!("创建文件 {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let mut written = 0u64;
    loop {
        let (kind, payload) = read_frame(&mut stream).context("读取文件块")?;
        match kind {
            KIND_FILE_CHUNK => {
                writer.write_all(&payload)?;
                written += payload.len() as u64;
            }
            KIND_FILE_DONE => break,
            _ => bail!("文件传输中出现未知帧 {kind}"),
        }
    }
    writer.flush()?;
    log(format!(
        "文件已保存: {} ({} 字节)",
        path.display(),
        written
    ));
    write_json(
        &mut stream,
        KIND_CONTROL,
        &ServerMsg::FileSaved {
            path: path.display().to_string(),
            bytes: written,
        },
    )
    .ok();
    Ok(())
}

fn sanitize_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("received.bin")
        .to_string()
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    for i in 1..10000 {
        let candidate = match ext {
            Some(ext) => dir.join(format!("{stem} ({i}).{ext}")),
            None => dir.join(format!("{stem} ({i})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}.bin", std::process::id()))
}

enum ControllerCmd {
    Msg(ClientMsg),
    Clip(ClipboardPayload),
    Stop,
    /// 立刻断开当前连接并重新连一次（键鼠自愈的最后手段）。
    Reconnect,
}

/// 控制端手里「最新的一帧」：一块持久画布 + 自上次被 UI 取走以来的脏瓦片。
///
/// 解码在读取线程完成，UI 线程只把脏瓦片贴进纹理，不必每帧整屏拷贝 8 MB。
pub struct FrameBoard {
    seq: AtomicU64,
    inner: Mutex<Board>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    stats: Arc<ControllerStats>,
}

struct Board {
    canvas: Option<Canvas>,
    patches: Vec<(u32, u32, u32, u32)>,
    full: bool,
}

/// 控制端统计：帧数/字节/解码耗时/RTT，全部由原子量承载，UI 线程随口读取。
#[derive(Default)]
pub struct ControllerStats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub decode_us: AtomicU64,
    pub rtt_us: AtomicU32,
    pub pongs: AtomicU64,
    pub ping_sent_us: AtomicU64,
}

/// 单调递增的微秒时间戳，只用来算 RTT。
fn now_micros() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_micros() as u64
}

impl FrameBoard {
    fn new(wake: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        Self {
            seq: AtomicU64::new(0),
            inner: Mutex::new(Board {
                canvas: None,
                patches: Vec::new(),
                full: false,
            }),
            wake,
            stats: Arc::new(ControllerStats::default()),
        }
    }

    /// 读取线程：把解码好的瓦片贴到画布上，并记下脏矩形。
    pub fn push(&self, grid: TileGrid, tiles: &[(u32, Vec<u8>)], bytes: usize, decode_us: u64) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        {
            let Board {
                canvas,
                patches,
                full,
            } = &mut *guard;
            let rebuild = match canvas.as_ref() {
                Some(existing) => existing.grid != grid,
                None => true,
            };
            if rebuild {
                *canvas = Some(Canvas::new(grid));
                patches.clear();
                *full = true;
            }
            let Some(canvas) = canvas.as_mut() else {
                return;
            };
            for (index, rgba) in tiles {
                let rect = grid.tile_rect(*index);
                canvas.blit_tile(*index, rgba);
                if !patches.contains(&rect) {
                    patches.push(rect);
                }
            }
            if patches.len() > 256 {
                // UI 落后太多，攒不动了，直接整帧重传。
                patches.clear();
                *full = true;
            }
        }
        self.seq.fetch_add(1, Ordering::Relaxed);
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.stats.decode_us.store(decode_us, Ordering::Relaxed);
        drop(guard);
        if let Some(wake) = self.wake.as_ref() {
            wake();
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    /// 只读地看一眼画布（自检用，不影响脏瓦片记账）。
    pub fn with_latest<R>(&self, f: impl FnOnce(&Canvas) -> R) -> Option<R> {
        let guard = self.inner.lock().ok()?;
        guard.canvas.as_ref().map(f)
    }

    pub fn stats(&self) -> &Arc<ControllerStats> {
        &self.stats
    }

    /// UI 线程：取走自上次以来的更新；`full == true` 表示画布重建/脏瓦片过多，需要整帧上传。
    pub fn take_update<R>(
        &self,
        f: impl FnOnce(&Canvas, &[(u32, u32, u32, u32)], bool) -> R,
    ) -> Option<R> {
        let mut guard = self.inner.lock().ok()?;
        let Board {
            canvas,
            patches,
            full,
        } = &mut *guard;
        if !*full && patches.is_empty() {
            return None;
        }
        let canvas = canvas.as_ref()?;
        let full_now = *full;
        *full = false;
        let patches = std::mem::take(patches);
        Some(f(canvas, &patches, full_now))
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.canvas = None;
            guard.patches.clear();
            guard.full = false;
        }
        self.seq.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct ControllerHandle {
    pub addr: SocketAddr,
    pub events: Receiver<NetEvent>,
    pub event_tx: Sender<NetEvent>,
    cmd_tx: Sender<ControllerCmd>,
    pub frames: Arc<FrameBoard>,
    pub connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl ControllerHandle {
    pub fn send(&self, msg: ClientMsg) -> bool {
        self.cmd_tx.send(ControllerCmd::Msg(msg)).is_ok()
    }
    pub fn set_quality(&self, quality: u8, fps: u8, max_width: u32) {
        let _ = self.send(ClientMsg::SetQuality {
            quality,
            fps,
            max_width,
        });
    }
    /// 告诉被控端还要不要注入键鼠（「只看画面」时传 false）。
    pub fn set_input(&self, enabled: bool) {
        let _ = self.send(ClientMsg::SetInput { enabled });
    }
    /// 要一次键鼠通道状态（控制端每 500 ms 探一次，用来判故障）。
    pub fn probe_input(&self) {
        let _ = self.send(ClientMsg::InputProbe);
    }
    /// 请被控端松开所有按下的键，并回一份当前状态（软修复）。
    pub fn resync_input(&self) {
        let _ = self.send(ClientMsg::ResyncInput);
    }
    /// 立刻断线重连（硬修复）。被控端会把旧会话顶掉，所以回来的一定是干净状态。
    pub fn reconnect_now(&self) {
        let _ = self.cmd_tx.send(ControllerCmd::Reconnect);
    }
    /// 立刻推一份剪贴板过去（Ctrl+V 之前用，避免粘到旧内容）。
    pub fn send_clip(&self, payload: ClipboardPayload) -> bool {
        self.cmd_tx.send(ControllerCmd::Clip(payload)).is_ok()
    }
    pub fn disconnect(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(ControllerCmd::Stop);
    }
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

pub fn connect_controller(
    addr: SocketAddr,
    clipboard: SharedClipboard,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<ControllerHandle> {
    let (event_tx, event_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let frames = Arc::new(FrameBoard::new(wake));
    let connected = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    {
        let event_tx = event_tx.clone();
        let frames = frames.clone();
        let connected = connected.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            controller_loop(addr, event_tx, cmd_rx, frames, connected, stop);
        });
    }
    {
        let clipboard = clipboard.clone();
        let cmd_tx = cmd_tx.clone();
        let connected = connected.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if connected.load(Ordering::Relaxed) && clipboard.enabled.load(Ordering::Relaxed) {
                    if let Some(payload) = clipboard.poll() {
                        if cmd_tx.send(ControllerCmd::Clip(payload)).is_err() {
                            break;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(80));
            }
        });
    }

    Ok(ControllerHandle {
        addr,
        events: event_rx,
        event_tx,
        cmd_tx,
        frames,
        connected,
        stop,
    })
}

fn controller_loop(
    addr: SocketAddr,
    event_tx: Sender<NetEvent>,
    cmd_rx: Receiver<ControllerCmd>,
    frames: Arc<FrameBoard>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let host = host_name();
    let mut last_error = String::new();
    while !stop.load(Ordering::Relaxed) {
        let attempt = TcpStream::connect_timeout(&addr, Duration::from_secs(3));
        let mut stream = match attempt {
            Ok(s) => s,
            Err(e) => {
                let text = format!("连接 {addr} 失败: {e}");
                if text != last_error {
                    let _ = event_tx.send(NetEvent::Log(text.clone()));
                    last_error = text;
                }
                sleep_interruptible(Duration::from_millis(1200), &stop);
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        let handshake = (|| -> Result<ServerHello> {
            write_json(
                &mut stream,
                KIND_CONTROL,
                &ControlHello {
                    version: APP_VERSION,
                    host: host.clone(),
                },
            )?;
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let (kind, payload) = read_frame(&mut stream)?;
            if kind != KIND_CONTROL {
                bail!("握手返回未知帧 {kind}");
            }
            let hello: ServerHello = parse_json(&payload)?;
            if hello.version != APP_VERSION {
                bail!("版本不匹配：控制端 {APP_VERSION} / 被控端 {}", hello.version);
            }
            stream.set_read_timeout(None).ok();
            Ok(hello)
        })();
        let hello = match handshake {
            Ok(h) => h,
            Err(e) => {
                let _ = event_tx.send(NetEvent::Log(format!("连接握手失败: {e:#}")));
                let _ = stream.shutdown(Shutdown::Both);
                sleep_interruptible(Duration::from_millis(1200), &stop);
                continue;
            }
        };
        last_error.clear();
        connected.store(true, Ordering::Relaxed);
        let _ = event_tx.send(NetEvent::Connected(hello.clone()));
        let _ = event_tx.send(NetEvent::Log(format!(
            "已连接 {} ({}x{})",
            hello.host, hello.width, hello.height
        )));

        let reader_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                let _ = event_tx.send(NetEvent::Log(format!("复制连接失败: {e}")));
                connected.store(false, Ordering::Relaxed);
                let _ = stream.shutdown(Shutdown::Both);
                sleep_interruptible(Duration::from_millis(1200), &stop);
                continue;
            }
        };
        let reader_alive = Arc::new(AtomicBool::new(true));
        {
            let event_tx = event_tx.clone();
            let frames = frames.clone();
            let reader_alive = reader_alive.clone();
            thread::spawn(move || {
                controller_reader(reader_stream, event_tx, frames, reader_alive);
            });
        }

        let stats = frames.stats().clone();
        let mut last_ping = Instant::now();
        let mut seen_pongs = stats.pongs.load(Ordering::Relaxed);
        let mut unanswered = 0u32;
        // 收到 Reconnect 时置位：跳过退避等待，立刻重连。
        let mut force_reconnect = false;
        while !stop.load(Ordering::Relaxed) && reader_alive.load(Ordering::Relaxed) {
            if last_ping.elapsed() > Duration::from_secs(3) {
                let pongs = stats.pongs.load(Ordering::Relaxed);
                if pongs > seen_pongs {
                    seen_pongs = pongs;
                    unanswered = 0;
                } else {
                    unanswered += 1;
                }
                // 三次心跳都没回应（约 9 秒）：TCP 可能还「连着」，但对端已经死了。
                if unanswered >= 3 {
                    let _ = event_tx.send(NetEvent::Log(
                        "心跳超时：连接已失效，正在重新连接".to_string(),
                    ));
                    break;
                }
                stats.ping_sent_us.store(now_micros(), Ordering::Relaxed);
                if write_json(&mut stream, KIND_CONTROL, &ClientMsg::Ping).is_err() {
                    break;
                }
                last_ping = Instant::now();
            }
            match cmd_rx.recv_timeout(Duration::from_millis(150)) {
                Ok(ControllerCmd::Msg(msg)) => {
                    if write_json(&mut stream, KIND_CONTROL, &msg).is_err() {
                        break;
                    }
                }
                Ok(ControllerCmd::Clip(payload)) => {
                    let result = match &payload {
                        ClipboardPayload::Text(text) => {
                            write_frame(&mut stream, KIND_CLIP_TEXT, text.as_bytes())
                        }
                        ClipboardPayload::Image(png) => {
                            write_frame(&mut stream, KIND_CLIP_IMAGE, png)
                        }
                    };
                    if result.is_err() {
                        break;
                    }
                }
                Ok(ControllerCmd::Stop) => break,
                Ok(ControllerCmd::Reconnect) => {
                    force_reconnect = true;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        connected.store(false, Ordering::Relaxed);
        reader_alive.store(false, Ordering::Relaxed);
        let _ = stream.shutdown(Shutdown::Both);
        let _ = event_tx.send(NetEvent::Disconnected("连接已断开".to_string()));
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !force_reconnect {
            sleep_interruptible(Duration::from_millis(800), &stop);
        }
    }
    connected.store(false, Ordering::Relaxed);
}

fn sleep_interruptible(duration: Duration, stop: &Arc<AtomicBool>) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn controller_reader(
    mut stream: TcpStream,
    event_tx: Sender<NetEvent>,
    frames: Arc<FrameBoard>,
    alive: Arc<AtomicBool>,
) {
    let stats = frames.stats().clone();
    while alive.load(Ordering::Relaxed) {
        let (kind, payload) = match read_frame(&mut stream) {
            Ok(v) => v,
            Err(e) => {
                let _ = event_tx.send(NetEvent::Log(format!("远程读取结束: {e:#}")));
                break;
            }
        };
        match kind {
            KIND_VIDEO => {
                let started = Instant::now();
                let Ok(message) = FrameMessage::parse(&payload) else {
                    continue;
                };
                let grid = message.grid;
                let tiles = decode_tiles(&message);
                let decode_us = started.elapsed().as_micros() as u64;
                if tiles.is_empty() && frames.seq() > 0 {
                    // 空帧（极少数情况下全部瓦片编码失败）直接跳过，保留上一帧。
                    continue;
                }
                frames.push(grid, &tiles, payload.len(), decode_us);
            }
            KIND_CONTROL => {
                if let Ok(msg) = parse_json::<ServerMsg>(&payload) {
                    match msg {
                        ServerMsg::FileSaved { path, bytes } => {
                            let _ = event_tx.send(NetEvent::FileSaved(path, bytes));
                        }
                        ServerMsg::Notice { text } => {
                            let _ = event_tx.send(NetEvent::Notice(text));
                        }
                        ServerMsg::InputStatus {
                            session,
                            received,
                            injected,
                            failed,
                            gated,
                            ok,
                        } => {
                            let _ = event_tx.send(NetEvent::InputStatus(InputStatusView {
                                session,
                                received,
                                injected,
                                failed,
                                gated,
                                ok,
                            }));
                        }
                    }
                }
            }
            KIND_CLIP_TEXT => {
                if let Ok(text) = String::from_utf8(payload) {
                    let _ = event_tx.send(NetEvent::Clipboard(ClipboardPayload::Text(text)));
                }
            }
            KIND_CLIP_IMAGE => {
                let _ = event_tx.send(NetEvent::Clipboard(ClipboardPayload::Image(payload)));
            }
            KIND_PONG => {
                stats.pongs.fetch_add(1, Ordering::Relaxed);
                let sent = stats.ping_sent_us.load(Ordering::Relaxed);
                let now = now_micros();
                if sent > 0 && now > sent && now - sent < 5_000_000 {
                    stats.rtt_us.store((now - sent) as u32, Ordering::Relaxed);
                }
            }
            KIND_PING => {}
            _ => {}
        }
    }
    alive.store(false, Ordering::Relaxed);
    let _ = stream.shutdown(Shutdown::Both);
}

/// 并行解码一帧里的所有 JPEG 瓦片 → RGBA 像素。
fn decode_tiles(message: &FrameMessage) -> Vec<(u32, Vec<u8>)> {
    let count = message.tiles.len();
    if count == 0 {
        return Vec::new();
    }
    let results: Mutex<Vec<(u32, Vec<u8>)>> = Mutex::new(Vec::with_capacity(count));
    let next = AtomicUsize::new(0);
    let workers = pipeline::worker_count().clamp(1, count);
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= count {
                    break;
                }
                let (tile_index, jpeg) = &message.tiles[index];
                let Ok(decoded) =
                    image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
                else {
                    continue;
                };
                let rgba = decoded.to_rgba8();
                if let Ok(mut guard) = results.lock() {
                    guard.push((*tile_index as u32, rgba.into_raw()));
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

pub fn send_file(addr: SocketAddr, path: PathBuf, event_tx: Sender<NetEvent>) {
    thread::spawn(move || {
        let result = (|| -> Result<()> {
            let mut file = File::open(&path).with_context(|| format!("打开 {}", path.display()))?;
            let size = file.metadata()?.len();
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file.bin")
                .to_string();
            let mut stream =
                TcpStream::connect_timeout(&addr, Duration::from_secs(5)).context("连接被控端")?;
            let _ = stream.set_nodelay(true);
            write_frame(
                &mut stream,
                KIND_FILE,
                &serde_json::to_vec(&FileHello { name, size })?,
            )?;
            let mut buf = vec![0u8; 128 * 1024];
            let mut sent = 0u64;
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                write_frame(&mut stream, KIND_FILE_CHUNK, &buf[..n])?;
                sent += n as u64;
            }
            write_frame(&mut stream, KIND_FILE_DONE, &[])?;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            if let Ok((kind, payload)) = read_frame(&mut stream) {
                if kind == KIND_CONTROL {
                    if let Ok(ServerMsg::FileSaved { path, bytes }) = parse_json(&payload) {
                        let _ = event_tx.send(NetEvent::FileSaved(path, bytes));
                        return Ok(());
                    }
                }
            }
            if sent != size {
                bail!("发送字节数不一致");
            }
            let _ = event_tx.send(NetEvent::Notice(format!("文件已发送（{sent} 字节）")));
            Ok(())
        })();
        if let Err(e) = result {
            let _ = event_tx.send(NetEvent::Log(format!("发送文件失败: {e:#}")));
        }
    });
}

pub fn scan_once(timeout: Duration) -> Vec<Peer> {
    let socket = match UdpSocket::bind(("0.0.0.0", 0)) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let _ = socket.set_broadcast(true);
    let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
    let targets = discovery_targets();
    for _ in 0..3 {
        for target in &targets {
            let _ = socket.send_to(DISCOVERY_MAGIC, target);
        }
        thread::sleep(Duration::from_millis(120));
    }
    let deadline = Instant::now() + timeout;
    let mut peers: Vec<Peer> = Vec::new();
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                if let Ok(reply) = serde_json::from_slice::<DiscoveryReply>(&buf[..len]) {
                    if reply.magic == String::from_utf8_lossy(DISCOVERY_MAGIC) {
                        let addr = SocketAddr::new(src.ip(), reply.port);
                        if !peers.iter().any(|p| p.addr == addr) {
                            peers.push(Peer {
                                name: reply.name,
                                addr,
                                version: reply.version,
                            });
                        }
                    }
                }
            }
            Err(_) => {}
        }
    }
    peers
}

fn discovery_targets() -> Vec<SocketAddr> {
    let mut targets = vec![SocketAddr::new(
        IpAddr::V4(Ipv4Addr::BROADCAST),
        DISCOVERY_PORT,
    )];
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let if_addrs::IfAddr::V4(v4) = &iface.addr {
                if v4.ip.is_loopback() || v4.ip.is_link_local() {
                    continue;
                }
                let broadcast = Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask));
                targets.push(SocketAddr::new(IpAddr::V4(broadcast), DISCOVERY_PORT));
            }
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

pub fn request_firewall_rules() -> Result<()> {
    let bat = std::env::temp_dir().join("lanlink_firewall.bat");
    let content = "@echo off\r\n\
netsh advfirewall firewall add rule name=\"LanLink TCP 47800\" dir=in action=allow protocol=TCP localport=47800 profile=any enable=yes\r\n\
netsh advfirewall firewall add rule name=\"LanLink UDP 47801\" dir=in action=allow protocol=UDP localport=47801 profile=any enable=yes\r\n";
    std::fs::write(&bat, content).context("写入防火墙脚本")?;
    let ps = format!(
        "Start-Process -FilePath cmd.exe -Verb RunAs -ArgumentList '/c \"{}\"'",
        bat.display()
    );
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .spawn()
        .context("请求管理员权限")?;
    Ok(())
}

pub fn autostart_enabled() -> bool {
    std::process::Command::new("reg")
        .args([
            "query",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            "/v",
            "LanLink",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    if enabled {
        let exe = std::env::current_exe().context("获取程序路径")?;
        let value = format!("\"{}\" --agent", exe.display());
        let status = std::process::Command::new("reg")
            .args([
                "add",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                "/v",
                "LanLink",
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ])
            .status()
            .context("运行 reg add")?;
        if !status.success() {
            bail!("写入开机自启失败");
        }
    } else {
        let status = std::process::Command::new("reg")
            .args([
                "delete",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                "/v",
                "LanLink",
                "/f",
            ])
            .status()
            .context("运行 reg delete")?;
        let _ = status;
    }
    Ok(())
}

