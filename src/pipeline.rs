//! 被控端的画面流水线：采集线程 + 编码线程。
//!
//! 采集线程：一次 BitBlt 全屏 → 逐瓦片与上一帧比较 → 只复制变化瓦片的数据。
//! 编码线程：把变化瓦片并行缩放/编码成 JPEG，合成一个 KIND_VIDEO 帧发出去。
//!
//! 两条线程重叠工作，采集（约 30 ms）与编码（约 2-4 ms）不再串行相加；
//! 变化很少的静态画面根本不占网络带宽。
//!
//! 队列深度固定为 1，而且发送是「阻塞」的：网络变慢时是采集降速（帧率下降），
//! 而不是把旧帧堆在内存里让画面延迟越拖越长；也不会像原来的 try_send+忙等那样
//! 白烧 CPU、还因为丢 job 而丢掉变化瓦片（表现为残影）。

use crate::capture::{encode_jpeg_rgb, scale_bgra_region, ScreenCapture};
use crate::frame::{scaled_size, source_rect, FrameMessage, TileGrid};
use crate::net::{send_raw, LogFn};
use crate::protocol::KIND_VIDEO;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quality {
    pub quality: u8,
    pub fps: u8,
    pub max_width: u32,
}

impl Default for Quality {
    fn default() -> Self {
        Self {
            quality: 72,
            fps: 60,
            max_width: 1920,
        }
    }
}

struct TileJob {
    /// 源屏幕像素（BGRA，紧排）。
    bgra: Vec<u8>,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
}

struct Job {
    grid: TileGrid,
    quality: u8,
    tiles: Vec<Option<TileJob>>,
}

/// 采集线程把最近一次的抓屏耗时（微秒）放在这里，编码线程打日志时读取。
struct Shared {
    alive: Arc<AtomicBool>,
    capture_us: AtomicU64,
}

/// 启动一套「采集 + 编码」线程。`alive` 置 false 时两条线程都会退出。
pub fn start_agent_pipeline(
    writer: Arc<Mutex<TcpStream>>,
    alive: Arc<AtomicBool>,
    quality: Arc<Mutex<Quality>>,
    log: LogFn,
) {
    let shared = Arc::new(Shared {
        alive: alive.clone(),
        capture_us: AtomicU64::new(0),
    });
    let (tx, rx) = mpsc::sync_channel::<Job>(1);
    {
        let shared = shared.clone();
        let log = log.clone();
        thread::spawn(move || capture_thread(shared, quality, log, tx));
    }
    thread::spawn(move || encode_thread(writer, shared, rx, log));
}

fn capture_thread(shared: Arc<Shared>, quality: Arc<Mutex<Quality>>, log: LogFn, tx: SyncSender<Job>) {
    let alive = shared.alive.clone();
    // 双屏机器上「只共享主屏」能省掉一半以上的抓屏时间：
    // GDI 抓屏耗时和要拷贝的像素数几乎成正比（实测 4.15 MP 约 50 ms）。
    let primary_only = std::env::args().any(|a| a == "--primary-only");
    let opened = if primary_only {
        crate::capture::primary_screen().and_then(ScreenCapture::from_rect)
    } else {
        ScreenCapture::new()
    };
    let mut capture = match opened {
        Ok(c) => c,
        Err(e) => {
            log(format!("屏幕采集初始化失败: {e:#}"));
            alive.store(false, Ordering::Relaxed);
            return;
        }
    };
    // 自检专用：没有桌面（锁屏）时用测试图案跑完整条链路。
    let test_pattern = std::env::var_os("LANLINK_TEST_PATTERN").is_some();
    let source = if primary_only { "主屏" } else { "虚拟屏" };
    log(if test_pattern {
        format!(
            "画面采集已启动（测试图案）：{source} {}x{}",
            capture.width(),
            capture.height()
        )
    } else {
        format!(
            "画面采集已启动：{source} {}x{}",
            capture.width(),
            capture.height()
        )
    });

    let mut grid = TileGrid::new(1, 1);
    let mut force_full = true;
    let mut last_error_log = Instant::now() - Duration::from_secs(10);
    let mut frame_index: u64 = 0;

    while alive.load(Ordering::Relaxed) {
        let started = Instant::now();
        let settings = *quality.lock().unwrap();

        let capture_started = Instant::now();
        let captured = if test_pattern {
            capture.fill_test_pattern(frame_index);
            frame_index = frame_index.wrapping_add(1);
            Ok(())
        } else {
            capture.capture()
        };
        if let Err(e) = captured {
            if last_error_log.elapsed() >= Duration::from_secs(5) {
                log(format!("抓屏失败: {e:#}"));
                last_error_log = Instant::now();
            }
            thread::sleep(Duration::from_millis(80));
            continue;
        }
        shared
            .capture_us
            .store(capture_started.elapsed().as_micros() as u64, Ordering::Relaxed);

        let (out_w, out_h) = scaled_size(capture.width(), capture.height(), settings.max_width);
        let next_grid = TileGrid::new(out_w, out_h);
        if next_grid != grid {
            grid = next_grid;
            force_full = true;
        }

        let mut job = Job {
            grid,
            quality: settings.quality,
            tiles: {
                let mut tiles: Vec<Option<TileJob>> = Vec::new();
                tiles.resize_with(grid.count() as usize, || None);
                tiles
            },
        };
        let mut changed = 0u32;
        for index in 0..grid.count() {
            let (dx, dy, dw, dh) = grid.tile_rect(index);
            let (sx, sy, sw, sh) = source_rect(
                dx,
                dy,
                dw,
                dh,
                grid.width,
                grid.height,
                capture.width(),
                capture.height(),
            );
            if !force_full && !capture.region_changed(sx, sy, sw, sh) {
                continue;
            }
            let mut bgra = Vec::new();
            capture.copy_region(sx, sy, sw, sh, &mut bgra);
            job.tiles[index as usize] = Some(TileJob {
                bgra,
                src_w: sw,
                src_h: sh,
                dst_w: dw,
                dst_h: dh,
            });
            changed += 1;
        }
        capture.swap();

        // 大面积变化（播视频、滚长网页、拖窗口）时自动降一档质量：
        // 编码、带宽、对端解码的压力都不会失控，帧率优先；
        // 静止画面和小改动仍按用户设置的高质量发送，文字依旧清晰。
        if changed > 0 {
            let total = grid.count().max(1);
            let ratio = changed as f32 / total as f32;
            let drop = if ratio > 0.75 {
                18
            } else if ratio > 0.45 {
                10
            } else if ratio > 0.2 {
                5
            } else {
                0
            };
            job.quality = settings.quality.saturating_sub(drop).max(30);
        }

        if changed > 0 || force_full {
            let was_full = force_full;
            // 阻塞发送：队列满代表编码/网络还没跟上，这时候等一等比丢帧更安全
            // （丢 frame 会把「变化瓦片」一起丢掉，控制端就残影了）。
            if tx.send(job).is_err() {
                return;
            }
            if was_full {
                // 整屏已经完整发出，之后只要增量瓦片。
                force_full = false;
            }
        }

        let interval = Duration::from_micros(1_000_000 / settings.fps.max(1) as u64);
        let elapsed = started.elapsed();
        if elapsed < interval {
            thread::sleep(interval - elapsed);
        }
    }
}

fn encode_thread(writer: Arc<Mutex<TcpStream>>, shared: Arc<Shared>, rx: Receiver<Job>, log: LogFn) {
    let alive = shared.alive.clone();
    let workers = worker_count();
    let mut frames = 0u64;
    let mut tiles = 0u64;
    let mut bytes = 0u64;
    let mut encode_us = 0u64;
    let mut send_us = 0u64;
    let mut max_send_us = 0u64;
    let mut last_report = Instant::now();
    let mut idle_reports = 0u64;

    loop {
        let job = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                idle_reports += 1;
                if last_report.elapsed() >= Duration::from_secs(5) {
                    report(
                        &log,
                        frames,
                        tiles,
                        bytes,
                        encode_us,
                        send_us,
                        max_send_us,
                        shared.capture_us.load(Ordering::Relaxed),
                        last_report.elapsed(),
                    );
                    frames = 0;
                    tiles = 0;
                    bytes = 0;
                    encode_us = 0;
                    send_us = 0;
                    max_send_us = 0;
                    last_report = Instant::now();
                }
                let _ = idle_reports;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if !alive.load(Ordering::Relaxed) {
            break;
        }

        let encode_started = Instant::now();
        let message = encode_job(job, workers);
        encode_us += encode_started.elapsed().as_micros() as u64;
        if message.tiles.is_empty() {
            continue;
        }
        tiles += message.tiles.len() as u64;
        let payload = message.encode();
        bytes += payload.len() as u64;
        frames += 1;
        let send_started = Instant::now();
        let ok = send_raw(&writer, &alive, KIND_VIDEO, &payload);
        let elapsed = send_started.elapsed().as_micros() as u64;
        send_us += elapsed;
        max_send_us = max_send_us.max(elapsed);
        if !ok {
            log("画面发送失败，已断开".to_string());
            break;
        }

        if last_report.elapsed() >= Duration::from_secs(5) {
            report(
                &log,
                frames,
                tiles,
                bytes,
                encode_us,
                send_us,
                max_send_us,
                shared.capture_us.load(Ordering::Relaxed),
                last_report.elapsed(),
            );
            frames = 0;
            tiles = 0;
            bytes = 0;
            encode_us = 0;
            send_us = 0;
            max_send_us = 0;
            last_report = Instant::now();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn report(
    log: &LogFn,
    frames: u64,
    tiles: u64,
    bytes: u64,
    encode_us: u64,
    send_us: u64,
    max_send_us: u64,
    capture_us: u64,
    window: Duration,
) {
    if frames == 0 {
        return;
    }
    let secs = window.as_secs_f32().max(0.001);
    let frames_f = frames as f32;
    log(format!(
        "画面 {:.1} fps，平均 {:.1} 块/帧，{:.1} MB/s｜最近一次抓屏 {:.1} ms，编码 {:.1} ms/帧，发送 {:.1} ms/帧（峰值 {:.1} ms）",
        frames_f / secs,
        tiles as f32 / frames_f,
        bytes as f32 / secs / (1024.0 * 1024.0),
        capture_us as f32 / 1000.0,
        encode_us as f32 / frames_f / 1000.0,
        send_us as f32 / frames_f / 1000.0,
        max_send_us as f32 / 1000.0,
    ));
}

fn encode_job(job: Job, workers: usize) -> FrameMessage {
    let count = job.tiles.len();
    let results: Mutex<Vec<(u16, Vec<u8>)>> = Mutex::new(Vec::with_capacity(count));
    let next = AtomicUsize::new(0);
    if count > 0 {
        let threads = workers.clamp(1, count);
        thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    let Some(tile) = job.tiles[index].as_ref() else {
                        continue;
                    };
                    let rgb = scale_bgra_region(
                        &tile.bgra, tile.src_w, tile.src_h, tile.dst_w, tile.dst_h,
                    );
                    if let Ok(jpeg) = encode_jpeg_rgb(&rgb, tile.dst_w, tile.dst_h, job.quality) {
                        if let Ok(mut guard) = results.lock() {
                            guard.push((index as u16, jpeg));
                        }
                    }
                });
            }
        });
    }
    let mut tiles = results.into_inner().unwrap_or_default();
    tiles.sort_unstable_by_key(|(index, _)| *index);
    FrameMessage {
        grid: job.grid,
        tiles,
    }
}

pub(crate) fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(2)
        .clamp(1, 8)
}
