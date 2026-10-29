//! 屏幕采集：DIB 双缓冲 + 逐瓦片变化检测 + 缩放/JPEG 编码工具。
//!
//! 采集是整条链路上最贵的一步（一次全屏 BitBlt 约 30 ms），所以这里把
//! 「读屏幕」和「判定/编码」拆开：BitBlt 只做一次，变化检测直接对比两块
//! DIB 内存（memcmp 级别），只有变化区域才会被复制出来做缩放和编码。

use crate::frame::{scaled_size, source_rect, TileGrid};
use anyhow::{bail, Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC, SelectObject,
    SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    CAPTUREBLT, COLORONCOLOR, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_REMOTESESSION, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

#[derive(Clone, Copy, Debug)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub fn virtual_screen() -> Result<ScreenRect> {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if width <= 0 || height <= 0 {
            bail!("invalid virtual screen size {width}x{height}");
        }
        Ok(ScreenRect {
            x,
            y,
            width: width as u32,
            height: height as u32,
        })
    }
}

/// 主显示器的矩形（Windows 规定主屏左上角就是 0,0）。
/// 双屏用户只共享主屏时，抓屏像素少一半到三分之二，帧率能明显上去。
pub fn primary_screen() -> Result<ScreenRect> {
    unsafe {
        let width = GetSystemMetrics(SM_CXSCREEN);
        let height = GetSystemMetrics(SM_CYSCREEN);
        if width <= 0 || height <= 0 {
            bail!("invalid primary screen size {width}x{height}");
        }
        Ok(ScreenRect {
            x: 0,
            y: 0,
            width: width as u32,
            height: height as u32,
        })
    }
}

/// 一块可直接写入的 DIB 缓冲（32bpp top-down BGRA）。
struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    bits: *mut u8,
    stride: usize,
    height: usize,
}

impl Dib {
    fn new(screen_dc: HDC, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let dc = compat_dc(screen_dc)?;
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: width * height * 4,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let mut bits: *mut c_void = ptr::null_mut();
            let bitmap = CreateDIBSection(
                screen_dc,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                ptr::null_mut(),
                0,
            );
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(dc);
                bail!("CreateDIBSection failed");
            }
            let old_bitmap = SelectObject(dc, bitmap);
            Ok(Self {
                dc,
                bitmap,
                old_bitmap,
                bits: bits as *mut u8,
                stride: width as usize * 4,
                height: height as usize,
            })
        }
    }

    fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits, self.stride * self.height) }
    }
}

fn compat_dc(screen_dc: HDC) -> Result<HDC> {
    unsafe {
        let dc = windows_sys::Win32::Graphics::Gdi::CreateCompatibleDC(screen_dc);
        if dc.is_null() {
            bail!("CreateCompatibleDC failed");
        }
        Ok(dc)
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            if !self.old_bitmap.is_null() {
                SelectObject(self.dc, self.old_bitmap);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
        }
    }
}

/// 虚拟屏采集器：两块 DIB 轮换，当前块是新画面，另一块是上一帧。
pub struct ScreenCapture {
    screen_dc: HDC,
    rect: ScreenRect,
    buffers: [Dib; 2],
    current: usize,
    have_prev: bool,
}

impl ScreenCapture {
    pub fn new() -> Result<Self> {
        Self::from_rect(virtual_screen()?)
    }

    pub fn from_rect(rect: ScreenRect) -> Result<Self> {
        unsafe {
            let screen_dc = GetDC(ptr::null_mut());
            if screen_dc.is_null() {
                bail!("GetDC failed");
            }
            let first = match Dib::new(screen_dc, rect.width, rect.height) {
                Ok(d) => d,
                Err(e) => {
                    ReleaseDC(ptr::null_mut(), screen_dc);
                    return Err(e);
                }
            };
            let second = match Dib::new(screen_dc, rect.width, rect.height) {
                Ok(d) => d,
                Err(e) => {
                    drop(first);
                    ReleaseDC(ptr::null_mut(), screen_dc);
                    return Err(e);
                }
            };
            Ok(Self {
                screen_dc,
                rect,
                buffers: [first, second],
                current: 0,
                have_prev: false,
            })
        }
    }

    pub fn width(&self) -> u32 {
        self.rect.width
    }

    pub fn height(&self) -> u32 {
        self.rect.height
    }

    /// 抓一帧到当前缓冲（一次 GDI 全屏拷贝，约 30-60 ms，取决于屏幕像素数）。
    ///
    /// 不再带 `CAPTUREBLT`：基准测试里它每帧要多花 5-7 ms，而它只影响
    /// 「分层窗口（半透明窗口）是否被合成进画面」这一点边缘情况。
    pub fn capture(&mut self) -> Result<()> {
        self.capture_flags(SRCCOPY)
    }

    /// 用指定的 BitBlt 标志抓一帧。`--bench-capture` 用它对比 `CAPTUREBLT` 的代价。
    pub fn capture_flags(&mut self, flags: u32) -> Result<()> {
        let buffer = &self.buffers[self.current];
        let ok = unsafe {
            BitBlt(
                buffer.dc,
                0,
                0,
                self.rect.width as i32,
                self.rect.height as i32,
                self.screen_dc,
                self.rect.x,
                self.rect.y,
                flags,
            )
        };
        if ok == 0 {
            bail!("BitBlt failed");
        }
        // 让 GDI 把这一批位图操作真正刷到内存，后面的比较才看到新像素。
        unsafe {
            GdiFlush();
        }
        Ok(())
    }

    /// 当前缓冲升级为「上一帧」，下次 capture 写另一块。
    pub fn swap(&mut self) {
        self.current ^= 1;
        self.have_prev = true;
    }

    /// 仅用于自检：不抓屏，直接在缓冲里画一条移动的色带，
    /// 这样变化检测、瓦片编码、传输、解码整条链路都能在没有桌面时被验证。
    pub fn fill_test_pattern(&mut self, frame: u64) {
        let buffer = &self.buffers[self.current];
        let data = unsafe {
            std::slice::from_raw_parts_mut(buffer.bits, buffer.stride * buffer.height)
        };
        let width = self.rect.width as usize;
        let height = self.rect.height as usize;
        if frame == 0 {
            for row in 0..height {
                for col in 0..width {
                    let offset = row * width * 4 + col * 4;
                    data[offset] = (col % 251) as u8;
                    data[offset + 1] = (row % 241) as u8;
                    data[offset + 2] = ((col + row) % 239) as u8;
                    data[offset + 3] = 255;
                }
            }
            return;
        }
        // 之后每帧只重画一条 128 像素高的横带（位置随帧号移动）。
        let band_h = 128usize.min(height);
        let top = ((frame as usize) * 96) % height.max(1);
        for row in 0..band_h {
            let y = (top + row) % height;
            for col in 0..width {
                let offset = y * width * 4 + col * 4;
                data[offset] = ((col + frame as usize) % 251) as u8;
                data[offset + 1] = ((y + frame as usize) % 241) as u8;
                data[offset + 2] = ((col ^ y) % 239) as u8;
                data[offset + 3] = 255;
            }
        }
    }

    /// 当前（新）画面与上一帧相比，这块矩形有没有变化。
    pub fn region_changed(&self, x: u32, y: u32, w: u32, h: u32) -> bool {
        if !self.have_prev {
            return true;
        }
        let cur = self.buffers[self.current].data();
        let prev = self.buffers[self.current ^ 1].data();
        let stride = self.buffers[self.current].stride;
        let (x, y, w, h) = match clamp_rect(x, y, w, h, self.rect.width, self.rect.height) {
            Some(v) => v,
            None => return false,
        };
        let row_bytes = w as usize * 4;
        for row in 0..h as usize {
            let start = (y as usize + row) * stride + x as usize * 4;
            let end = start + row_bytes;
            if end > cur.len() || end > prev.len() {
                return true;
            }
            if cur[start..end] != prev[start..end] {
                return true;
            }
        }
        false
    }

    /// 复制当前缓冲的一块区域（紧排的 BGRA，行宽 = w * 4）。
    pub fn copy_region(&self, x: u32, y: u32, w: u32, h: u32, dst: &mut Vec<u8>) {
        let stride = self.buffers[self.current].stride;
        let row_bytes = w as usize * 4;
        dst.clear();
        dst.resize(row_bytes * h as usize, 0);
        let src = self.buffers[self.current].data();
        for row in 0..h as usize {
            let start = (y as usize + row) * stride + x as usize * 4;
            let end = start + row_bytes;
            if end > src.len() {
                return;
            }
            let dst_start = row * row_bytes;
            dst[dst_start..dst_start + row_bytes].copy_from_slice(&src[start..end]);
        }
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        unsafe {
            if !self.screen_dc.is_null() {
                ReleaseDC(ptr::null_mut(), self.screen_dc);
            }
        }
    }
}

fn clamp_rect(x: u32, y: u32, w: u32, h: u32, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    if x >= width || y >= height {
        return None;
    }
    let w = w.min(width - x);
    let h = h.min(height - y);
    if w == 0 || h == 0 {
        return None;
    }
    Some((x, y, w, h))
}

/// BGRA 区域 → RGB，并按面积平均缩放（box filter）。输出紧排的 RGB。
pub fn scale_bgra_region(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (
        src_w as usize,
        src_h as usize,
        dst_w.max(1) as usize,
        dst_h.max(1) as usize,
    );
    if sw == 0 || sh == 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; dw * dh * 3];
    if src.len() < sw * sh * 4 {
        return out;
    }

    if sw == dw && sh == dh {
        for (dst, src_px) in out.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
            dst[0] = src_px[2];
            dst[1] = src_px[1];
            dst[2] = src_px[0];
        }
        return out;
    }

    for dy in 0..dh {
        let sy0 = dy * sh / dh;
        let sy1 = (((dy + 1) * sh).div_ceil(dh)).min(sh).max(sy0 + 1);
        let rows = sy1 - sy0;
        for dx in 0..dw {
            let sx0 = dx * sw / dw;
            let sx1 = (((dx + 1) * sw).div_ceil(dw)).min(sw).max(sx0 + 1);
            let mut r: u32 = 0;
            let mut g: u32 = 0;
            let mut b: u32 = 0;
            for sy in sy0..sy1 {
                let row_start = sy * sw * 4 + sx0 * 4;
                let row = &src[row_start..row_start + (sx1 - sx0) * 4];
                for px in row.chunks_exact(4) {
                    b += px[0] as u32;
                    g += px[1] as u32;
                    r += px[2] as u32;
                }
            }
            let count = ((sx1 - sx0) * rows) as u32;
            let offset = (dy * dw + dx) * 3;
            out[offset] = (r / count) as u8;
            out[offset + 1] = (g / count) as u8;
            out[offset + 2] = (b / count) as u8;
        }
    }
    out
}

/// RGB 像素编码成一块 JPEG。
pub fn encode_jpeg_rgb(rgb: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(16 * 1024);
    let encoder = JpegEncoder::new_with_quality(&mut out, quality.clamp(20, 95));
    encoder
        .write_image(rgb, width, height, ExtendedColorType::Rgb8)
        .context("jpeg encode")?;
    Ok(out)
}

/// `lanlink.exe --bench-capture`：把抓屏的真实耗时写进 `_bench_capture.log`，
/// 用来判断「帧数上不去」到底卡在 GDI 抓屏、变化检测还是编码。
pub fn bench_capture() {
    let started = std::time::Instant::now();
    let mut lines: Vec<String> = Vec::new();
    let rect = match virtual_screen() {
        Ok(r) => r,
        Err(e) => {
            lines.push(format!("虚拟屏查询失败: {e:#}"));
            write_bench(&lines);
            return;
        }
    };
    let pixels = rect.width as u64 * rect.height as u64;
    lines.push(format!(
        "虚拟屏 {}x{} = {:.2} MP，每帧 BGRA {:.1} MB",
        rect.width,
        rect.height,
        pixels as f64 / 1_000_000.0,
        pixels as f64 * 4.0 / (1024.0 * 1024.0)
    ));
    lines.push(format!(
        "CPU 线程数: {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    ));
    // 远程会话（RDP）下抓屏会走远程显示驱动，通常慢好几倍，这个数字能解释一半的差异。
    lines.push(format!(
        "会话类型: {}",
        if unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0 {
            "远程桌面（RDP）—— 抓屏会明显变慢"
        } else {
            "本地控制台"
        }
    ));
    let mut capture = match ScreenCapture::new() {
        Ok(c) => c,
        Err(e) => {
            lines.push(format!("初始化采集失败: {e:#}"));
            write_bench(&lines);
            return;
        }
    };

    const ROUNDS: u32 = 30;
    for (name, flags) in [
        ("SRCCOPY|CAPTUREBLT", SRCCOPY | CAPTUREBLT),
        ("SRCCOPY", SRCCOPY),
    ] {
        let mut total = 0.0f64;
        let mut best = f64::MAX;
        let mut worst: f64 = 0.0;
        for _ in 0..ROUNDS {
            let t = std::time::Instant::now();
            if capture.capture_flags(flags).is_err() {
                break;
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            total += ms;
            best = best.min(ms);
            worst = worst.max(ms);
        }
        lines.push(format!(
            "BitBlt[{name}]: 平均 {:.2} ms，最快 {:.2} ms，最慢 {:.2} ms",
            total / ROUNDS as f64,
            best,
            worst
        ));
    }

    let (out_w, out_h) = scaled_size(rect.width, rect.height, 1920);

    // 只抓一角（1/4 像素面积）用来确认 BitBlt 的代价是否与面积成正比。
    {
        let buffer = &capture.buffers[capture.current];
        let (w, h) = (rect.width / 2, rect.height / 2);
        let mut total = 0.0f64;
        let mut best = f64::MAX;
        for _ in 0..ROUNDS {
            let t = std::time::Instant::now();
            let ok = unsafe {
                BitBlt(
                    buffer.dc,
                    0,
                    0,
                    w as i32,
                    h as i32,
                    capture.screen_dc,
                    capture.rect.x,
                    capture.rect.y,
                    SRCCOPY,
                )
            };
            unsafe { GdiFlush() };
            total += t.elapsed().as_secs_f64() * 1000.0;
            best = best.min(total);
            if ok == 0 {
                break;
            }
        }
        lines.push(format!(
            "BitBlt[1/4 面积] {w}x{h}: 平均 {:.2} ms",
            total / ROUNDS as f64
        ));
    }

    // 直接按目标分辨率抓：让 GDI 的 HALFTONE 做缩放，省掉后面的软件缩放。
    for (mode, label) in [(HALFTONE, "HALFTONE"), (COLORONCOLOR, "COLORONCOLOR")] {
        let Ok(small) = Dib::new(capture.screen_dc, out_w, out_h) else {
            continue;
        };
        unsafe {
            SetStretchBltMode(small.dc, mode);
            SetBrushOrgEx(small.dc, 0, 0, ptr::null_mut());
        }
        let mut total = 0.0f64;
        let mut best = f64::MAX;
        let mut worst: f64 = 0.0;
        for _ in 0..ROUNDS {
            let t = std::time::Instant::now();
            let ok = unsafe {
                StretchBlt(
                    small.dc,
                    0,
                    0,
                    out_w as i32,
                    out_h as i32,
                    capture.screen_dc,
                    capture.rect.x,
                    capture.rect.y,
                    capture.rect.width as i32,
                    capture.rect.height as i32,
                    SRCCOPY,
                )
            };
            unsafe { GdiFlush() };
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            total += ms;
            best = best.min(ms);
            worst = worst.max(ms);
            if ok == 0 {
                break;
            }
        }
        lines.push(format!(
            "StretchBlt[{label} → {out_w}x{out_h}]: 平均 {:.2} ms，最快 {:.2} ms，最慢 {:.2} ms",
            total / ROUNDS as f64,
            best,
            worst
        ));
    }

    // 备选路径：先 BitBlt 到「兼容位图」（显存），再用 GetDIBits 读回一帧。
    // 目的：判断 59 ms 是花在 BitBlt 的 GPU 合成上，还是花在 DIB 的读回上。
    {
        unsafe {
            let mem_dc = windows_sys::Win32::Graphics::Gdi::CreateCompatibleDC(capture.screen_dc);
            let bitmap = windows_sys::Win32::Graphics::Gdi::CreateCompatibleBitmap(
                capture.screen_dc,
                rect.width as i32,
                rect.height as i32,
            );
            if !mem_dc.is_null() && !bitmap.is_null() {
                let old = SelectObject(mem_dc, bitmap);
                let mut blit_total = 0.0f64;
                let mut blit_best = f64::MAX;
                let mut read_total = 0.0f64;
                let mut read_best = f64::MAX;
                let mut info: BITMAPINFO = std::mem::zeroed();
                info.bmiHeader = BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: rect.width as i32,
                    biHeight: -(rect.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    biSizeImage: rect.width * rect.height * 4,
                    ..std::mem::zeroed()
                };
                let mut buffer = vec![0u8; rect.width as usize * rect.height as usize * 4];
                for _ in 0..10 {
                    let t = std::time::Instant::now();
                    let ok = BitBlt(
                        mem_dc,
                        0,
                        0,
                        rect.width as i32,
                        rect.height as i32,
                        capture.screen_dc,
                        rect.x,
                        rect.y,
                        SRCCOPY,
                    );
                    GdiFlush();
                    let blit = t.elapsed().as_secs_f64() * 1000.0;
                    if ok == 0 {
                        break;
                    }
                    let lines_read = windows_sys::Win32::Graphics::Gdi::GetDIBits(
                        mem_dc,
                        bitmap,
                        0,
                        rect.height,
                        buffer.as_mut_ptr() as *mut c_void,
                        &mut info,
                        DIB_RGB_COLORS,
                    );
                    let read = t.elapsed().as_secs_f64() * 1000.0 - blit;
                    if lines_read == 0 {
                        break;
                    }
                    blit_total += blit;
                    blit_best = blit_best.min(blit);
                    read_total += read;
                    read_best = read_best.min(read);
                }
                let _ = old;
                DeleteObject(bitmap);
                DeleteDC(mem_dc);
                lines.push(format!(
                    "兼容位图路径：BitBlt(GPU) 平均 {:.2} ms，GetDIBits 读回 平均 {:.2} ms（最快 {:.2} ms）",
                    blit_total / 10.0,
                    read_total / 10.0,
                    read_best
                ));
            }
        }
    }

    let grid = TileGrid::new(out_w, out_h);
    let mut total_scan = 0.0f64;
    let mut tiles_changed = 0u64;
    const JOB_ROUNDS: u32 = 10;
    let mut buf = Vec::new();
    for _ in 0..JOB_ROUNDS {
        if capture.capture().is_err() {
            break;
        }
        capture.swap();
        let mut changed = 0u32;
        let t = std::time::Instant::now();
        for index in 0..grid.count() {
            let (dx, dy, dw, dh) = grid.tile_rect(index);
            let (sx, sy, sw, sh) = source_rect(
                dx,
                dy,
                dw,
                dh,
                grid.width,
                grid.height,
                rect.width,
                rect.height,
            );
            if !capture.region_changed(sx, sy, sw, sh) {
                continue;
            }
            capture.copy_region(sx, sy, sw, sh, &mut buf);
            changed += 1;
        }
        total_scan += t.elapsed().as_secs_f64() * 1000.0;
        tiles_changed += changed as u64;
    }
    lines.push(format!(
        "变化检测+复制（输出 {}x{} / {} 块）：平均 {:.2} ms，平均变化 {:.1} 块",
        out_w,
        out_h,
        grid.count(),
        total_scan / JOB_ROUNDS as f64,
        tiles_changed as f64 / JOB_ROUNDS as f64
    ));
    lines.push(format!("总耗时 {:.2} s", started.elapsed().as_secs_f64()));
    write_bench(&lines);
}

fn write_bench(lines: &[String]) {
    let path = std::env::current_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("_bench_capture.log");
    let body = lines.join("\r\n");
    let _ = std::fs::write(&path, body.clone());
    println!("{body}");
    println!("写入 {}", path.display());
}
