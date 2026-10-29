//! 统一的深色高对比度主题。
//!
//! 上一版是两个问题的叠加：
//!
//! 1. `ctx.set_visuals(Visuals::dark())` 在 egui 0.31 里只写「当前主题」的那一份样式；
//!    而 eframe 随后会把主题偏好恢复成「跟随系统」。用户的 Windows 是浅色主题，
//!    于是界面实际用的是**浅色**视觉（深灰文字 #434343）。
//! 2. 顶栏/底栏用的是 `Frame::default()`，它的 `fill` 是透明的，于是面板没画背景，
//!    露出的是 eframe 的 clear color（半透明近黑 12,12,12）。
//!
//! 结果就是「近黑的底色 + 深灰的文字」——文字几乎看不见。
//!
//! 现在：显式锁定深色主题、把自定义 Style 同时写进 dark/light 两份样式、
//! 每个面板都显式给 fill，并且覆盖 `App::clear_color`，不再依赖系统主题。

use eframe::egui;
use std::sync::Arc;

// ---- 调色板：所有颜色只从这里取，避免各处随手写 RGB 导致对比度失控 ----

/// 窗口/画布底色。
pub const BG: egui::Color32 = egui::Color32::from_rgb(15, 17, 22);
/// 顶栏、底栏。
pub const BAR: egui::Color32 = egui::Color32::from_rgb(25, 28, 35);
/// 卡片、日志区。
pub const CARD: egui::Color32 = egui::Color32::from_rgb(31, 35, 44);
/// 输入框内部。
pub const FIELD: egui::Color32 = egui::Color32::from_rgb(12, 14, 18);
/// 分隔线 / 描边。
pub const LINE: egui::Color32 = egui::Color32::from_rgb(62, 70, 86);
/// 正文（在 BAR 上对比度约 13:1）。
pub const TEXT: egui::Color32 = egui::Color32::from_rgb(233, 237, 245);
/// 次要文字（在 BAR 上对比度约 6.4:1，仍高于 WCAG AA 的 4.5:1）。
pub const TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(163, 173, 191);
/// 强调色：链接、选中、可点击提示。
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(104, 176, 255);
/// 正常 / 已连接。
pub const OK: egui::Color32 = egui::Color32::from_rgb(94, 224, 149);
/// 警告（只看画面、需要留意）。
pub const WARN: egui::Color32 = egui::Color32::from_rgb(255, 202, 87);
/// 错误（键鼠通道异常、VAC）。
pub const ERR: egui::Color32 = egui::Color32::from_rgb(255, 110, 110);
/// 按钮常态填充。
const BTN: egui::Color32 = egui::Color32::from_rgb(46, 52, 65);
/// 按钮悬停填充。
const BTN_HOVER: egui::Color32 = egui::Color32::from_rgb(62, 70, 88);
/// 按钮按下填充。
const BTN_ACTIVE: egui::Color32 = egui::Color32::from_rgb(82, 132, 196);

/// 顶栏 / 底栏用的面板框：显式 fill，绝不留透明。
pub fn bar_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(BAR)
        .inner_margin(egui::Margin::symmetric(12, 8))
}

/// 卡片框。
pub fn card_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(CARD)
        .stroke(egui::Stroke::new(1.0, LINE))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(14, 12))
}

/// 顶部工具条里的「胶囊」：浅底 + 高对比文字。
pub fn pill_frame(fill: egui::Color32) -> egui::Frame {
    egui::Frame::default()
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(9))
        .inner_margin(egui::Margin::symmetric(9, 3))
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    // 全局兜底：任何没有显式指定颜色的文字都是亮色，彻底消灭「深灰写深灰」。
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = CARD;
    v.code_bg_color = FIELD;
    v.window_stroke = egui::Stroke::new(1.0, LINE);
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT.linear_multiply(0.42);
    v.selection.stroke = egui::Stroke::new(1.0, TEXT);
    v.text_cursor.stroke = egui::Stroke::new(2.0, TEXT);
    // 滚动条别太暗。
    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.weak_bg_fill = CARD;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, LINE);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT_DIM);

    v.widgets.inactive.bg_fill = BTN;
    v.widgets.inactive.weak_bg_fill = BTN;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, LINE);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);

    v.widgets.hovered.bg_fill = BTN_HOVER;
    v.widgets.hovered.weak_bg_fill = BTN_HOVER;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);

    v.widgets.active.bg_fill = BTN_ACTIVE;
    v.widgets.active.weak_bg_fill = BTN_ACTIVE;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);

    v.widgets.open.bg_fill = BTN_HOVER;
    v.widgets.open.weak_bg_fill = BTN_HOVER;
    v.widgets.open.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0, TEXT);
    v
}

fn style() -> egui::Style {
    let mut style = egui::Style::default();
    style.visuals = visuals();
    // 字号整体抬高：原来的 12.5px 正文在 1080p 上就是「看不清」。
    style.text_styles = [
        (
            egui::TextStyle::Heading,
            egui::FontId::new(22.0, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Body,
            egui::FontId::new(15.5, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Button,
            egui::FontId::new(15.5, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Small,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Monospace,
            egui::FontId::new(14.0, egui::FontFamily::Monospace),
        ),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(11.0, 5.0);
    style.spacing.interact_size.y = 27.0;
    style.spacing.slider_width = 118.0;
    style.spacing.combo_width = 96.0;
    style.spacing.indent = 18.0;
    style
}

/// 应用主题。`update` 每帧调用也是安全的（只在被外部改回去时才重新写一次）。
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    // 锁定深色：不跟随系统（用户机器是浅色系统，正是上一版翻车的根因）。
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_style_of(egui::Theme::Dark, style());
    ctx.set_style_of(egui::Theme::Light, style());
    ctx.set_style(style());
}

/// 主题是否被外部（系统主题切换等）覆盖掉了。
pub fn needs_reapply(ctx: &egui::Context) -> bool {
    ctx.style().visuals.panel_fill != BG
        || !matches!(ctx.theme(), egui::Theme::Dark)
        || ctx.style().visuals.override_text_color != Some(TEXT)
}

/// 显式声明面板底色，给中央区域用。
pub fn central_frame() -> egui::Frame {
    egui::Frame::default().fill(BG)
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyhbd.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\simsun.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".to_owned(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .insert(0, "cjk".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

/// 一行「标签 : 值」，标签用次要色、值用正文色——比全用同一种灰色清楚得多。
pub fn labeled(ui: &mut egui::Ui, label: &str, value: impl Into<String>) {
    ui.label(egui::RichText::new(label).color(TEXT_DIM));
    ui.label(egui::RichText::new(value.into()).color(TEXT).strong());
}

/// 状态胶囊。
pub fn pill(ui: &mut egui::Ui, dot: egui::Color32, text: &str, fill: egui::Color32) -> egui::Response {
    pill_frame(fill)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, dot);
                ui.label(egui::RichText::new(text).color(TEXT).size(14.0));
            });
        })
        .response
}
