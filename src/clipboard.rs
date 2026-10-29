use anyhow::{Context, Result};
use arboard::{Clipboard, ImageData};
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;

#[derive(Clone, Debug)]
pub enum ClipboardPayload {
    Text(String),
    Image(Vec<u8>),
}

struct State {
    last_seq: u32,
    last_hash: u64,
    suppress_until: Instant,
}

#[derive(Clone)]
pub struct SharedClipboard {
    state: Arc<Mutex<State>>,
    pub enabled: Arc<AtomicBool>,
}

impl SharedClipboard {
    pub fn new() -> Self {
        let seq = unsafe { GetClipboardSequenceNumber() };
        Self {
            state: Arc::new(Mutex::new(State {
                last_seq: seq,
                last_hash: 0,
                suppress_until: Instant::now(),
            })),
            enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// 让下一次 `poll()` 无视「内容和序列号都没变」，重新读一次剪贴板。
    ///
    /// 用在两个地方：刚连上（把当前剪贴板补发给新连接）、按下 Ctrl+V 之前
    /// （保证被控端拿到的一定是最新内容，而不是重连前的旧内容）。
    pub fn invalidate(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.last_seq = 0;
            state.last_hash = 0;
        }
    }

    pub fn poll(&self) -> Option<ClipboardPayload> {
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == 0 {
            return None;
        }
        {
            let mut state = self.state.lock().ok()?;
            if seq == state.last_seq {
                return None;
            }
            if Instant::now() < state.suppress_until {
                state.last_seq = seq;
                return None;
            }
            if !self.enabled.load(Ordering::Relaxed) {
                state.last_seq = seq;
                return None;
            }
        }

        let mut clipboard = Clipboard::new().ok()?;
        let payload = if let Ok(image) = clipboard.get_image() {
            let mut png = Vec::new();
            let encoder = PngEncoder::new(&mut png);
            if encoder
                .write_image(
                    &image.bytes,
                    image.width as u32,
                    image.height as u32,
                    ExtendedColorType::Rgba8,
                )
                .is_ok()
            {
                ClipboardPayload::Image(png)
            } else if let Ok(text) = clipboard.get_text() {
                ClipboardPayload::Text(text)
            } else {
                return None;
            }
        } else if let Ok(text) = clipboard.get_text() {
            ClipboardPayload::Text(text)
        } else {
            return None;
        };

        let hash = payload_hash(&payload);
        let mut state = self.state.lock().ok()?;
        state.last_seq = seq;
        if hash == state.last_hash {
            return None;
        }
        state.last_hash = hash;
        Some(payload)
    }

    pub fn apply(&self, payload: &ClipboardPayload) -> Result<()> {
        let mut clipboard = Clipboard::new().context("open clipboard")?;
        match payload {
            ClipboardPayload::Text(text) => {
                clipboard.set_text(text.clone()).context("set clipboard text")?;
            }
            ClipboardPayload::Image(png) => {
                let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
                    .context("decode clipboard png")?
                    .to_rgba8();
                clipboard
                    .set_image(ImageData {
                        width: image.width() as usize,
                        height: image.height() as usize,
                        bytes: Cow::Owned(image.into_raw()),
                    })
                    .context("set clipboard image")?;
            }
        }
        if let Ok(mut state) = self.state.lock() {
            state.last_seq = unsafe { GetClipboardSequenceNumber() };
            state.last_hash = payload_hash(payload);
            state.suppress_until = Instant::now() + Duration::from_millis(700);
        }
        Ok(())
    }
}

fn payload_hash(payload: &ClipboardPayload) -> u64 {
    let bytes: &[u8] = match payload {
        ClipboardPayload::Text(text) => text.as_bytes(),
        ClipboardPayload::Image(png) => png,
    };
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
