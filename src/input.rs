//! 被控端的键鼠注入。
//!
//! 所有注入都走 SendInput。批量操作（一段文本、一次全部释放）合成一次调用，
//! 这样被控端的应用看到的是一串连续、无抖动的输入，而不是被线程调度切碎的事件。

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
};

// ---- 注入健康度 ----
//
// 控制端靠这三个数判断「键鼠到底通不通」：注入成功/失败计数 + 最近一次是否成功。
// 没有它们的时候，被控端的 SendInput 被 UIPI 挡住（比如焦点在管理员权限的窗口、
// UAC 安全桌面、锁屏界面）时，控制端完全无感，用户只会看到「点了没反应」。

static INJECTED: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static LAST_OK: AtomicBool = AtomicBool::new(true);

/// (累计成功注入的事件数, 累计失败次数, 最近一次语义输入是否被系统接受)
pub fn health() -> (u64, u64, bool) {
    (
        INJECTED.load(Ordering::Relaxed),
        FAILED.load(Ordering::Relaxed),
        LAST_OK.load(Ordering::Relaxed),
    )
}

fn record(ok: bool, count: usize) {
    if ok {
        INJECTED.fetch_add(count as u64, Ordering::Relaxed);
        LAST_OK.store(true, Ordering::Relaxed);
    } else {
        FAILED.fetch_add(1, Ordering::Relaxed);
        LAST_OK.store(false, Ordering::Relaxed);
    }
}

/// 不计数：鼠标移动 / 滚轮频率太高，会把「刚才按键失败了」这件事冲掉。
fn send(input: INPUT) -> bool {
    unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) == 1 }
}

/// 会计数的单条注入（鼠标按键、键盘按键）。
fn send_tracked(input: INPUT) -> bool {
    let ok = send(input);
    record(ok, 1);
    ok
}

/// 一次提交多个输入事件（顺序与数组一致，中间不会被别的注入插队）。
fn send_all(inputs: &[INPUT]) -> usize {
    if inputs.is_empty() {
        return 0;
    }
    unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) as usize }
}

fn mouse_input(flags: u32, dx: i32, dy: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 给注入的按键补上真实扫描码。
///
/// 真实键盘事件里 wVk 和 wScan 都是有效的：Chromium 系应用（Chrome / Electron /
/// VS Code / 各种网页应用）用扫描码填 `KeyboardEvent.code`，DirectInput 游戏和
/// 一些控制台程序也只看扫描码。只填 wVk、wScan 留 0 时，这些程序会把按键当成
/// 「未知键」，表现为「Ctrl+C 按了没反应」。
fn scan_code(vk: u16) -> u16 {
    unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 }
}

fn key_input(vk: u16, down: bool) -> INPUT {
    // 方向键 / Insert / Delete / Home / End / PgUp / PgDn / Win / Apps / 小键盘除号回车
    // 必须带 EXTENDEDKEY，否则被控端会把它们当成小键盘数字。
    let extended = matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C | 0x6F | 0x90);
    let mut flags = 0u32;
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan_code(vk),
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_input(unit: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: if up {
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                } else {
                    KEYEVENTF_UNICODE
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 归一化坐标（0..1，相对整个虚拟桌面）→ 绝对鼠标移动。
pub fn mouse_move(nx: f32, ny: f32) {
    if !nx.is_finite() || !ny.is_finite() {
        return;
    }
    let dx = (nx.clamp(0.0, 1.0) * 65535.0).round() as i32;
    let dy = (ny.clamp(0.0, 1.0) * 65535.0).round() as i32;
    let flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    send(mouse_input(flags, dx, dy, 0));
}

fn button_flag(button: u8, down: bool) -> Option<u32> {
    Some(match (button, down) {
        (0, true) => MOUSEEVENTF_LEFTDOWN,
        (0, false) => MOUSEEVENTF_LEFTUP,
        (1, true) => MOUSEEVENTF_RIGHTDOWN,
        (1, false) => MOUSEEVENTF_RIGHTUP,
        (2, true) => MOUSEEVENTF_MIDDLEDOWN,
        (2, false) => MOUSEEVENTF_MIDDLEUP,
        (3, true) => MOUSEEVENTF_XDOWN,
        (3, false) => MOUSEEVENTF_XUP,
        (4, true) => MOUSEEVENTF_XDOWN,
        (4, false) => MOUSEEVENTF_XUP,
        _ => return None,
    })
}

fn button_data(button: u8) -> u32 {
    match button {
        3 => 1, // XBUTTON1（后退）
        4 => 2, // XBUTTON2（前进）
        _ => 0,
    }
}

pub fn mouse_button(button: u8, down: bool) {
    let Some(flag) = button_flag(button, down) else {
        return;
    };
    send_tracked(mouse_input(flag, 0, 0, button_data(button)));
}

pub fn wheel(delta: i32) {
    if delta == 0 {
        return;
    }
    send(mouse_input(MOUSEEVENTF_WHEEL, 0, 0, delta as u32));
}

/// 发送一次按键事件，返回 SendInput 是否接受。
pub fn key(vk: u16, down: bool) -> bool {
    send_tracked(key_input(vk, down))
}

/// 按 UTF-16 单元注入文本；每 256 个字符（512 个事件）提交一次。
///
/// 返回实际被系统接受的事件数（0 表示注入被拒绝）。
pub fn text(value: &str) -> usize {
    if value.is_empty() {
        return 0;
    }
    let units: Vec<u16> = value.encode_utf16().collect();
    let mut submitted = 0;
    for chunk in units.chunks(256) {
        let mut inputs = Vec::with_capacity(chunk.len() * 2);
        for &unit in chunk {
            inputs.push(unicode_input(unit, false));
            inputs.push(unicode_input(unit, true));
        }
        let accepted = send_all(&inputs);
        record(accepted == inputs.len(), accepted);
        submitted += accepted;
    }
    submitted
}

/// 诊断用：往当前前台窗口注入「F24 + 一段文本 + Ctrl+A + Ctrl+C」。
///
/// 用 `lanlink.exe --test-inject` 触发，配合记事本 + 读剪贴板可以验证
/// SendInput 链路（文本、修饰键顺序、扫描码）是否正常，不需要两台机器。
/// 返回逐步诊断报告，写入 `_inject_result.txt`。
pub fn dev_inject_test(full: bool) -> String {
    use std::time::{Duration, Instant};

    const TEXT: &str = "LanLink inject test 123";
    let start = Instant::now();
    let mut report = String::new();
    let step = |report: &mut String, label: &str, ok: bool| {
        report.push_str(&format!(
            "{label}: ok={ok} at {}ms\n",
            start.elapsed().as_millis()
        ));
    };

    // 先打一个真人几乎不会按的键（F24），用来确认 SendInput 到底通不通。
    // 打两次：一次在进程刚启动、一次在 1.2 秒后，用来区分「启动太早」还是「根本发不出去」。
    let f24 = send_all(&[key_input(0x87, true), key_input(0x87, false)]);
    report.push_str(&format!(
        "F24 early: submitted={f24}/2 at {}ms\n",
        start.elapsed().as_millis()
    ));
    std::thread::sleep(Duration::from_millis(1200));
    let f24 = send_all(&[key_input(0x87, true), key_input(0x87, false)]);
    report.push_str(&format!(
        "F24 late: submitted={f24}/2 at {}ms\n",
        start.elapsed().as_millis()
    ));
    if !full {
        return report;
    }
    std::thread::sleep(Duration::from_millis(120));

    let units = TEXT.encode_utf16().count();
    let submitted = text(TEXT);
    report.push_str(&format!(
        "text: submitted={submitted}/{} at {}ms\n",
        units * 2,
        start.elapsed().as_millis()
    ));
    std::thread::sleep(Duration::from_millis(150));

    step(&mut report, "Ctrl down", key(0x11, true));
    step(&mut report, "A down", key(0x41, true));
    step(&mut report, "A up", key(0x41, false));
    std::thread::sleep(Duration::from_millis(80));
    step(&mut report, "C down", key(0x43, true));
    step(&mut report, "C up", key(0x43, false));
    std::thread::sleep(Duration::from_millis(80));
    step(&mut report, "Ctrl up", key(0x11, false));
    report
}

/// 连接断开 / 控制被关闭时调用：把可能卡在按下状态的键鼠全部弹起，
/// 否则被控端会「一直按着 Ctrl」或者「鼠标左键按死」。
pub fn release_all() {
    let mut inputs: Vec<INPUT> = Vec::with_capacity(24);
    for vk in [
        0x11u16, 0x10, 0x12, // Ctrl / Shift / Alt
        0xA2, 0xA3, 0xA0, 0xA1, 0xA4, 0xA5, // 左右修饰键
        0x5B, 0x5C, // Win
        0x5D, // Apps
    ] {
        inputs.push(key_input(vk, false));
    }
    inputs.push(mouse_input(MOUSEEVENTF_LEFTUP, 0, 0, 0));
    inputs.push(mouse_input(MOUSEEVENTF_RIGHTUP, 0, 0, 0));
    inputs.push(mouse_input(MOUSEEVENTF_MIDDLEUP, 0, 0, 0));
    inputs.push(mouse_input(MOUSEEVENTF_XUP, 0, 0, 1));
    inputs.push(mouse_input(MOUSEEVENTF_XUP, 0, 0, 2));
    send_all(&inputs);
}
