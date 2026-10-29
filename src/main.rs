#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod capture;
mod clipboard;
mod frame;
mod input;
mod net;
mod pipeline;
mod protocol;
mod selftest;
mod theme;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--self-test") {
        let ok = selftest::run();
        std::process::exit(if ok { 0 } else { 1 });
    }
    if args.iter().any(|a| a == "--bench-capture") {
        capture::bench_capture();
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--test-inject") {
        // 诊断用：往当前前台窗口注入按键，验证 SendInput 链路。
        // 默认只打一个无害的 F24；加 --test-inject-text 才注入文本 + Ctrl+A + Ctrl+C。
        let full = args.iter().any(|a| a == "--test-inject-text");
        let report = input::dev_inject_test(full);
        print!("{report}");
        let _ = std::fs::write("_inject_result.txt", &report);
        std::process::exit(0);
    }
    let mode = if args.iter().any(|a| a == "--agent") {
        app::Mode::Agent
    } else {
        app::Mode::Controller
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title(match mode {
                app::Mode::Agent => "LanLink 被控端",
                app::Mode::Controller => "LanLink 控制端",
            }),
        ..Default::default()
    };
    eframe::run_native(
        "LanLink",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, mode)) as Box<dyn eframe::App>)),
    )
}
