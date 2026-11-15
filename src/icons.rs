//! Toolbar icons (Haiku OS icon set, MIT, see assets/icons/LICENSE) embedded in
//! the binary, and the icon-above-label toolbar button.

use eframe::egui::{self, vec2, Align2, Color32, FontId, Rect, Response, Sense, TextureHandle, Ui};

pub struct Icons {
    pub back: TextureHandle,
    pub forward: TextureHandle,
    pub up: TextureHandle,
    pub recent: TextureHandle,
    pub refresh: TextureHandle,
    pub view: TextureHandle,
    pub rotate_left: TextureHandle,
    pub rotate_right: TextureHandle,
    pub rename: TextureHandle,
    pub delete: TextureHandle,
    pub explorer: TextureHandle,
    pub more: TextureHandle,
    pub folder: TextureHandle,
    pub star: TextureHandle,
    pub settings: TextureHandle,
}

fn load(ctx: &egui::Context, name: &str, png: &[u8]) -> TextureHandle {
    let img = image::load_from_memory(png).expect("embedded icon").to_rgba8();
    let ci = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
    ctx.load_texture(
        format!("icon/{name}"),
        ci,
        egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR },
    )
}

macro_rules! icons {
    ($ctx:expr, $($name:ident),*) => {
        Icons { $($name: load($ctx, stringify!($name), include_bytes!(concat!("../assets/icons/", stringify!($name), ".png")))),* }
    };
}

impl Icons {
    pub fn new(ctx: &egui::Context) -> Self {
        icons!(ctx, back, forward, up, recent, refresh, view, rotate_left, rotate_right, rename, delete, explorer, more, folder, star, settings)
    }
}

pub const ICON: f32 = 20.0;
pub const H: f32 = 40.0;

/// Toolbar button: icon on top, label below. Hover lights it up, pressing pushes
/// the icon down a little so the click is felt; disabled ones are greyed out.
pub fn tool_button(ui: &mut Ui, icon: &TextureHandle, label: &str, enabled: bool) -> Response {
    let font = FontId::proportional(11.5);
    let text_w = ui.fonts(|f| f.layout_no_wrap(label.to_owned(), font.clone(), Color32::BLACK).size().x);
    let w = (text_w + 6.0).max(34.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, H), sense);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let pressed = enabled && resp.is_pointer_button_down_on();
    if enabled && (resp.hovered() || pressed) {
        let (bg, line) = if pressed {
            (Color32::from_rgb(196, 222, 250), Color32::from_rgb(110, 160, 220))
        } else {
            (Color32::from_rgb(229, 241, 255), Color32::from_rgb(170, 205, 245))
        };
        p.rect_filled(rect, 5.0f32, bg);
        p.rect_stroke(rect, 5.0f32, egui::Stroke::new(1.0f32, line), egui::StrokeKind::Inside);
    }
    let push = if pressed { 1.5 } else { 0.0 };
    let icon_rect = Rect::from_center_size(rect.center_top() + vec2(0.0, 4.0 + ICON / 2.0 + push), vec2(ICON, ICON));
    let tint = if enabled { Color32::WHITE } else { Color32::from_white_alpha(70) };
    p.image(icon.id(), icon_rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), tint);
    let color = if enabled { Color32::from_gray(25) } else { Color32::from_gray(160) };
    p.text(rect.center_bottom() - vec2(0.0, 8.0 - push), Align2::CENTER_CENTER, label, font, color);
    resp
}

// ---------------------------------------------------------------- popup menus
// Design values from a GPT (Codex) proposal, applied only inside menus so the
// rest of the UI keeps the default look.

const MENU_TEXT: Color32 = Color32::from_rgb(36, 48, 60);
const MENU_HOVER: Color32 = Color32::from_rgb(229, 241, 255);
const MENU_PRESS: Color32 = Color32::from_rgb(212, 231, 252);
const MENU_KEY: Color32 = Color32::from_rgb(111, 124, 140);
const MENU_LINE: Color32 = Color32::from_rgb(230, 234, 240);

/// Frame look shared by every popup: white, 8px corners, soft shadow.
pub fn apply_menu_frame(ctx: &egui::Context) {
    ctx.style_mut(|s| {
        s.visuals.menu_corner_radius = egui::CornerRadius::same(8);
        s.visuals.window_stroke = egui::Stroke::new(1.0f32, Color32::from_rgb(213, 218, 225));
        s.visuals.popup_shadow = egui::epaint::Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(32) };
        s.spacing.menu_margin = egui::Margin::same(6);
    });
}

/// Call at the top of a menu: flat 30px items with a light-blue hover.
pub fn style_menu(ui: &mut Ui, min_width: f32) {
    ui.set_min_width(min_width);
    let sp = ui.spacing_mut();
    sp.button_padding = vec2(10.0, 5.0);
    sp.item_spacing.y = 2.0;
    sp.icon_spacing = 8.0;
    let v = ui.visuals_mut();
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = egui::CornerRadius::same(4);
        w.bg_stroke = egui::Stroke::NONE;
        w.fg_stroke = egui::Stroke::new(1.0f32, MENU_TEXT);
        w.expansion = 0.0;
    }
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.hovered.weak_bg_fill = MENU_HOVER;
    v.widgets.open.weak_bg_fill = MENU_HOVER;
    v.widgets.active.weak_bg_fill = MENU_PRESS;
    v.selection.bg_fill = MENU_HOVER;
    v.selection.stroke = egui::Stroke::new(1.0f32, MENU_TEXT);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0f32, MENU_LINE);
}

/// Menu row: 16px icon column (kept empty when there is no icon), label,
/// shortcut on the right.
pub fn menu_item(ui: &mut Ui, icon: Option<&TextureHandle>, text: impl Into<egui::WidgetText>, shortcut: &str, enabled: bool) -> Response {
    let size = vec2(16.0, 16.0);
    let image = match icon {
        Some(t) => egui::Image::new((t.id(), size)),
        // the font atlas, fully transparent: reserves the icon column
        None => egui::Image::new((egui::TextureId::Managed(0), size)).tint(Color32::TRANSPARENT),
    };
    let button = egui::Button::image_and_text(image.fit_to_exact_size(size), text)
        .shortcut_text(egui::RichText::new(shortcut).size(12.0).color(MENU_KEY))
        .min_size(vec2(ui.available_width(), 30.0));
    ui.add_enabled(enabled, button)
}

pub fn menu_sep(ui: &mut Ui) {
    ui.add(egui::Separator::default().spacing(8.0));
}
