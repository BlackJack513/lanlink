//! 瓦片化的视频帧：一块画布 + 若干个独立编码的 JPEG 瓦片。
//!
//! 被控端把画面按固定尺寸切成瓦片，每帧只重新编码「发生了变化的瓦片」；
//! 控制端维护一块持久画布，把收到的瓦片贴上去，再整块上传成纹理。
//! LAN 通信方式（TCP + 1 字节 kind + u32 长度 + 负载）完全不变，
//! 变化的只是 KIND_VIDEO 负载的内部结构。

use anyhow::{bail, Result};

pub const TILE_W: u32 = 256;
pub const TILE_H: u32 = 144;

const MAGIC: [u8; 4] = *b"LFV2";
const HEADER_LEN: usize = 4 + 2 + 2 + 4;
const ENTRY_LEN: usize = 2 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileGrid {
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
}

impl TileGrid {
    pub fn new(width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            cols: width.div_ceil(TILE_W),
            rows: height.div_ceil(TILE_H),
        }
    }

    pub fn count(&self) -> u32 {
        self.cols * self.rows
    }

    /// 第 `index` 块瓦片在画布上的 (x, y, w, h)。
    pub fn tile_rect(&self, index: u32) -> (u32, u32, u32, u32) {
        let col = index % self.cols;
        let row = index / self.cols;
        let x = (col * TILE_W).min(self.width - 1);
        let y = (row * TILE_H).min(self.height - 1);
        let w = TILE_W.min(self.width - x);
        let h = TILE_H.min(self.height - y);
        (x, y, w, h)
    }
}

/// 输出画布尺寸：按 `max_width` 等比缩小，只缩不放。
pub fn scaled_size(src_w: u32, src_h: u32, max_width: u32) -> (u32, u32) {
    let src_w = src_w.max(1);
    let src_h = src_h.max(1);
    if max_width == 0 || src_w <= max_width {
        return (src_w, src_h);
    }
    let out_w = max_width.max(320);
    let out_h = ((src_h as u64 * out_w as u64) / src_w as u64).max(1) as u32;
    (out_w, out_h)
}

/// 输出瓦片矩形对应的源屏幕像素矩形（向下取整/向上取整，保证覆盖且不越界）。
pub fn source_rect(
    dx: u32,
    dy: u32,
    dw: u32,
    dh: u32,
    out_w: u32,
    out_h: u32,
    src_w: u32,
    src_h: u32,
) -> (u32, u32, u32, u32) {
    let out_w = out_w.max(1) as u64;
    let out_h = out_h.max(1) as u64;
    let sx0 = (dx as u64 * src_w as u64 / out_w) as u32;
    let sy0 = (dy as u64 * src_h as u64 / out_h) as u32;
    let sx1 = ((((dx + dw) as u64 * src_w as u64).div_ceil(out_w)) as u32).min(src_w);
    let sy1 = ((((dy + dh) as u64 * src_h as u64).div_ceil(out_h)) as u32).min(src_h);
    let sx1 = sx1.max(sx0 + 1).min(src_w);
    let sy1 = sy1.max(sy0 + 1).min(src_h);
    (sx0, sy0, sx1 - sx0, sy1 - sy0)
}

#[derive(Debug)]
pub struct FrameMessage {
    pub grid: TileGrid,
    pub tiles: Vec<(u16, Vec<u8>)>,
}

impl FrameMessage {
    pub fn encode(&self) -> Vec<u8> {
        let body: usize = self
            .tiles
            .iter()
            .map(|(_, jpeg)| ENTRY_LEN + jpeg.len())
            .sum();
        let mut out = Vec::with_capacity(HEADER_LEN + body);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&(self.grid.width as u16).to_be_bytes());
        out.extend_from_slice(&(self.grid.height as u16).to_be_bytes());
        out.extend_from_slice(&(self.tiles.len() as u32).to_be_bytes());
        for (index, jpeg) in &self.tiles {
            out.extend_from_slice(&index.to_be_bytes());
            out.extend_from_slice(&(jpeg.len() as u32).to_be_bytes());
            out.extend_from_slice(jpeg);
        }
        out
    }

    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_LEN || data[..4] != MAGIC {
            bail!("视频帧头无效");
        }
        let width = u16::from_be_bytes([data[4], data[5]]) as u32;
        let height = u16::from_be_bytes([data[6], data[7]]) as u32;
        let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
        if width == 0 || height == 0 {
            bail!("视频帧尺寸无效");
        }
        let grid = TileGrid::new(width, height);
        if count > grid.count() as usize {
            bail!("视频帧瓦片数量超过画布容量");
        }
        let mut tiles = Vec::with_capacity(count);
        let mut offset = HEADER_LEN;
        for _ in 0..count {
            if offset + ENTRY_LEN > data.len() {
                bail!("视频帧数据被截断");
            }
            let index = u16::from_be_bytes([data[offset], data[offset + 1]]);
            let len =
                u32::from_be_bytes([data[offset + 2], data[offset + 3], data[offset + 4], data[offset + 5]])
                    as usize;
            offset += ENTRY_LEN;
            if offset + len > data.len() {
                bail!("视频帧瓦片数据被截断");
            }
            tiles.push((index, data[offset..offset + len].to_vec()));
            offset += len;
        }
        Ok(Self { grid, tiles })
    }
}

/// 控制端维护的整屏画布（RGBA，alpha 恒为 255）。
pub struct Canvas {
    pub grid: TileGrid,
    pub rgba: Vec<u8>,
}

impl Canvas {
    pub fn new(grid: TileGrid) -> Self {
        let mut rgba = vec![0u8; grid.width as usize * grid.height as usize * 4];
        for pixel in rgba.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Self { grid, rgba }
    }

    /// 把一块瓦片的 RGBA 像素贴到画布上。
    pub fn blit_tile(&mut self, index: u32, tile: &[u8]) {
        let (x, y, w, h) = self.grid.tile_rect(index);
        let stride = self.grid.width as usize * 4;
        let tile_stride = w as usize * 4;
        if tile.len() < tile_stride * h as usize {
            return;
        }
        for row in 0..h as usize {
            let dst = (y as usize + row) * stride + x as usize * 4;
            let src = row * tile_stride;
            let Some(dst_end) = dst.checked_add(tile_stride) else {
                return;
            };
            if dst_end > self.rgba.len() {
                return;
            }
            self.rgba[dst..dst_end].copy_from_slice(&tile[src..src + tile_stride]);
        }
    }
}
