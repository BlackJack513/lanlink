use anyhow::Context;
use crate::capture::{encode_jpeg_rgb, scale_bgra_region, ScreenCapture};
use crate::clipboard::SharedClipboard;
use crate::frame::{Canvas, FrameMessage, TileGrid};
use crate::net::{self, NetEvent};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TEST_PORT: u16 = 47890;

pub fn run() -> bool {
    let log_path = std::env::current_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("_selftest.log");
    let log_file = Arc::new(Mutex::new(
        std::fs::File::create(&log_path).ok(),
    ));
    let log = {
        let log_file = log_file.clone();
        Arc::new(move |msg: String| {
            println!("{msg}");
            if let Ok(mut guard) = log_file.lock() {
                if let Some(file) = guard.as_mut() {
                    let _ = writeln!(file, "{msg}");
                    let _ = file.flush();
                }
            }
        })
    };
    match run_inner(log.clone()) {
        Ok(()) => {
            log("SELFTEST PASS".to_string());
            true
        }
        Err(e) => {
            log(format!("SELFTEST FAIL: {e:#}"));
            false
        }
    }
}

fn run_inner(log: net::LogFn) -> anyhow::Result<()> {
    log(format!("self-test start, port {TEST_PORT}"));
    codec_check(&log)?;
    let capture_available = probe_capture(&log);
    if !capture_available {
        log("抓屏不可用：改用测试图案跑完整条视频链路".to_string());
        std::env::set_var("LANLINK_TEST_PATTERN", "1");
    }
    let test_download = std::env::current_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(format!("_selftest_downloads_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_download);
    std::fs::create_dir_all(&test_download)?;
    std::env::set_var("LANLINK_DOWNLOAD_DIR", &test_download);
    let clipboard = SharedClipboard::new();
    clipboard.set_enabled(false);
    let agent = net::start_agent(TEST_PORT, clipboard.clone(), log.clone(), false)?;
    std::thread::sleep(Duration::from_millis(200));

    let addr: SocketAddr = format!("127.0.0.1:{TEST_PORT}").parse()?;
    let controller = net::connect_controller(addr, clipboard.clone(), None)?;
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut connected = false;
    let mut got_frame = false;
    let mut last_seq = 0u64;
    while Instant::now() < deadline && (!connected || !got_frame) {
        while let Ok(event) = controller.events.try_recv() {
            match event {
                NetEvent::Connected(hello) => {
                    log(format!(
                        "handshake ok: {} {}x{}",
                        hello.host, hello.width, hello.height
                    ));
                    connected = true;
                }
                NetEvent::Log(text) => log(format!("controller: {text}")),
                _ => {}
            }
        }
        let seq = controller.frames.seq();
        if seq != last_seq {
            last_seq = seq;
            let covered = controller
                .frames
                .with_latest(|canvas| {
                    let expected =
                        canvas.grid.width as usize * canvas.grid.height as usize * 4;
                    canvas.rgba.len() == expected && canvas.rgba[3] == 255
                })
                .unwrap_or(false);
            if covered {
                log(format!("video frame ok, seq {seq}"));
                got_frame = true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !connected {
        anyhow::bail!("handshake timeout");
    }
    if !got_frame {
        anyhow::bail!("no video frame received");
    }

    // 「只看画面」：被控端收到 SetInput(false) 后必须回执确认（并停止注入键鼠）。
    controller.set_input(false);
    let view_deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_view_only = false;
    while Instant::now() < view_deadline && !saw_view_only {
        while let Ok(event) = controller.events.try_recv() {
            match event {
                NetEvent::Notice(text) | NetEvent::Log(text) => {
                    log(format!("controller: {text}"));
                    if text.contains("只看画面") {
                        saw_view_only = true;
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::ensure!(saw_view_only, "SetInput(false) 没有传到被控端");
    controller.set_input(true);
    log("只看画面切换：被控端已确认".to_string());

    // 键鼠自愈通道：控制端每 500 ms 探一次，被控端必须回一条 InputStatus。
    // 这条通道没了，控制端就没法判断「键鼠是不是卡住了」。
    controller.probe_input();
    let probe_deadline = Instant::now() + Duration::from_secs(3);
    let mut status: Option<net::InputStatusView> = None;
    while Instant::now() < probe_deadline && status.is_none() {
        while let Ok(event) = controller.events.try_recv() {
            if let NetEvent::InputStatus(view) = event {
                status = Some(view);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = status.ok_or_else(|| anyhow::anyhow!("InputProbe 没有回执，键鼠自愈通道不可用"))?;
    anyhow::ensure!(status.session >= 1, "会话编号异常: {}", status.session);
    log(format!(
        "键鼠健康通道 ok：会话 #{} 收到 {} 注入 {} 失败 {}",
        status.session, status.received, status.injected, status.failed
    ));

    // 增量纹理上传：首帧必须整帧；之后画面还在动时应该只带脏瓦片。
    let first_update = controller
        .frames
        .take_update(|_, patches, full| (patches.len(), full));
    anyhow::ensure!(
        matches!(first_update, Some((_, true))),
        "首帧没有走整帧上传路径: {first_update:?}"
    );
    let mut patch_updates = 0u32;
    let mut extra_full = 0u32;
    let patch_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < patch_deadline && patch_updates < 5 {
        if let Some((count, full)) = controller
            .frames
            .take_update(|_, patches, full| (patches.len(), full))
        {
            if full {
                extra_full += 1;
            } else if count > 0 {
                patch_updates += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    log(format!(
        "纹理增量更新：{patch_updates} 次脏瓦片，{extra_full} 次整帧"
    ));

    // 接收端实测帧率：验证管线能稳定跑满默认上限。
    // 窗口取 6 秒，刚好覆盖被控端那个 5 秒一次的耗时分解报告。
    let fps_seq = controller.frames.seq();
    let fps_started = Instant::now();
    std::thread::sleep(Duration::from_millis(6000));
    let fps_frames = controller.frames.seq() - fps_seq;
    let fps_secs = fps_started.elapsed().as_secs_f64().max(0.001);
    log(format!(
        "接收端实测 {:.1} fps（{fps_frames} 帧 / {fps_secs:.2} s）",
        fps_frames as f64 / fps_secs
    ));

    let temp = std::env::current_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(format!("_selftest_upload_{}.bin", std::process::id()));
    log(format!("writing test file {}", temp.display()));
    let mut data = Vec::with_capacity(300 * 1024);
    for i in 0..300 * 1024u32 {
        data.push((i.wrapping_mul(31) % 251) as u8);
    }
    std::fs::write(&temp, &data)
        .with_context(|| format!("写入测试文件 {}", temp.display()))?;
    log(format!("sending test file {}", temp.display()));
    let (tx, rx) = mpsc::channel();
    net::send_file(addr, temp.clone(), tx);
    let file_deadline = Instant::now() + Duration::from_secs(15);
    let mut saved: Option<(String, u64)> = None;
    while Instant::now() < file_deadline {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(200)) {
            match event {
                NetEvent::FileSaved(path, bytes) => {
                    saved = Some((path, bytes));
                    break;
                }
                NetEvent::Log(text) => log(format!("file: {text}")),
                NetEvent::Notice(text) => log(format!("file: {text}")),
                _ => {}
            }
        }
    }
    let (saved_path, saved_bytes) = saved.ok_or_else(|| anyhow::anyhow!("file transfer timeout"))?;
    log(format!("file saved: {saved_path} ({saved_bytes} bytes)"));
    if saved_bytes != data.len() as u64 {
        anyhow::bail!("file size mismatch: {} != {}", saved_bytes, data.len());
    }
    if let Ok(meta) = std::fs::metadata(&saved_path) {
        if meta.len() != data.len() as u64 {
            anyhow::bail!("saved file size mismatch");
        }
    }
    let _ = std::fs::remove_file(&saved_path);
    let _ = std::fs::remove_file(&temp);
    let _ = std::fs::remove_dir_all(&test_download);

    controller.disconnect();
    agent.shutdown();
    log("self-test finished".to_string());
    Ok(())
}

/// 不依赖屏幕的编解码自检：构造一帧瓦片，走一遍 编码 → 帧打包 → 解析 → 解码 → 拼接。
fn codec_check(log: &net::LogFn) -> anyhow::Result<()> {
    let grid = TileGrid::new(1920, 1080);
    let started = Instant::now();
    let mut tiles = Vec::with_capacity(grid.count() as usize);
    for index in 0..grid.count() {
        let (tx, ty, tw, th) = grid.tile_rect(index);
        let mut bgra = vec![0u8; (tw * th * 4) as usize];
        for row in 0..th as usize {
            for col in 0..tw as usize {
                let o = (row * tw as usize + col) * 4;
                bgra[o] = (tx as usize + col) as u8;
                bgra[o + 1] = (ty as usize + row) as u8;
                bgra[o + 2] = (tx as usize + col + ty as usize + row) as u8;
                bgra[o + 3] = 255;
            }
        }
        let rgb = scale_bgra_region(&bgra, tw, th, tw, th);
        let jpeg = encode_jpeg_rgb(&rgb, tw, th, 72)?;
        tiles.push((index as u16, jpeg));
    }
    let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
    let payload = FrameMessage { grid, tiles }.encode();
    let parsed = FrameMessage::parse(&payload)?;
    let mut canvas = Canvas::new(parsed.grid);
    let decode_started = Instant::now();
    for (index, jpeg) in &parsed.tiles {
        let image = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
            .context("jpeg decode")?
            .to_rgba8();
        canvas.blit_tile(*index as u32, image.as_raw());
    }
    let decode_ms = decode_started.elapsed().as_secs_f64() * 1000.0;
    let expected = (grid.width * grid.height * 4) as usize;
    anyhow::ensure!(canvas.rgba.len() == expected, "canvas size mismatch");
    anyhow::ensure!(canvas.rgba[3] == 255, "canvas alpha not filled");
    log(format!(
        "编解码自检：{} 块 {}x{}，编码 {:.1} ms，负载 {} KB，解码拼接 {:.1} ms",
        parsed.tiles.len(),
        grid.width,
        grid.height,
        encode_ms,
        payload.len() / 1024,
        decode_ms
    ));
    Ok(())
}

/// 当前会话能否抓屏（锁屏 / 无桌面时会失败）。
fn probe_capture(log: &net::LogFn) -> bool {
    match ScreenCapture::new().and_then(|mut capture| {
        capture.capture()?;
        Ok(())
    }) {
        Ok(()) => {
            log("抓屏可用".to_string());
            true
        }
        Err(e) => {
            log(format!("抓屏不可用: {e:#}"));
            false
        }
    }
}
