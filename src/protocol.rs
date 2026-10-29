use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;

/// 版本历史：
/// 2 → 3：新增 `SetInput`（控制端可切换「只看画面」）。
/// 3 → 4：新增键鼠通道自愈（`InputProbe` / `ResyncInput` / `ServerMsg::InputStatus`），
///        被控端改为「新会话顶掉旧会话」而不是拒绝连接。握手仍按版本号严格匹配。
pub const APP_VERSION: u32 = 4;
pub const DEFAULT_PORT: u16 = 47800;
pub const DISCOVERY_PORT: u16 = 47801;
pub const DISCOVERY_MAGIC: &[u8] = b"LANLINK_DISCOVER_V1";
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

pub const KIND_CONTROL: u8 = 1;
pub const KIND_VIDEO: u8 = 2;
pub const KIND_FILE: u8 = 3;
pub const KIND_CLIP_TEXT: u8 = 10;
pub const KIND_CLIP_IMAGE: u8 = 11;
pub const KIND_FILE_CHUNK: u8 = 13;
pub const KIND_FILE_DONE: u8 = 14;
pub const KIND_PING: u8 = 20;
pub const KIND_PONG: u8 = 21;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ControlHello {
    pub version: u32,
    pub host: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServerHello {
    pub version: u32,
    pub host: String,
    pub width: u32,
    pub height: u32,
    pub clipboard_enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    MouseMove { x: f32, y: f32 },
    MouseButton { button: u8, down: bool },
    Wheel { delta: i32 },
    Key { vk: u16, down: bool },
    Text { text: String },
    Ping,
    SetQuality { quality: u8, fps: u8, max_width: u32 },
    /// 控制端是否允许往被控端注入键鼠。false = 「只看画面」。
    SetInput { enabled: bool },
    /// 键鼠通道健康探测：被控端收到后立刻回一条 `ServerMsg::InputStatus`。
    ///
    /// 控制端每 500 ms 发一次，用 `received` 是否增长来判断「指令到底进没进去」。
    InputProbe,
    /// 强制自愈：被控端松开所有键鼠、清掉卡住的注入状态，并回一条 `InputStatus`。
    ///
    /// 这是「键鼠突然失灵」时控制端按 F8 / 点「修复键鼠」发的东西。
    ResyncInput,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    FileSaved { path: String, bytes: u64 },
    Notice { text: String },
    /// 键鼠通道健康报告。
    InputStatus {
        /// 被控端本次会话的编号（控制端用它判断「是不是换了一个会话」）。
        session: u64,
        /// 被控端实际收到的键鼠消息总数。
        received: u64,
        /// 成功注入到系统的键鼠事件数。
        injected: u64,
        /// `SendInput` 被系统拒绝的次数（UIPI / 安全桌面 / 桌面切换）。
        failed: u64,
        /// 当前是否允许注入（「只看画面」时为 false）。
        gated: bool,
        /// 最近一次注入是否成功。
        ok: bool,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileHello {
    pub name: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DiscoveryReply {
    pub magic: String,
    pub name: String,
    pub port: u16,
    pub version: u32,
}

pub fn write_frame(stream: &mut TcpStream, kind: u8, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME {
        bail!("frame too large: {}", payload.len());
    }
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header).context("write frame header")?;
    if !payload.is_empty() {
        stream.write_all(payload).context("write frame body")?;
    }
    stream.flush().ok();
    Ok(())
}

pub fn read_frame(stream: &mut TcpStream) -> Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).context("read frame header")?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).context("read frame body")?;
    }
    Ok((header[0], payload))
}

pub fn write_json<T: Serialize>(stream: &mut TcpStream, kind: u8, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    write_frame(stream, kind, &bytes)
}

pub fn parse_json<T: DeserializeOwned>(payload: &[u8]) -> Result<T> {
    serde_json::from_slice(payload).context("parse json message")
}
