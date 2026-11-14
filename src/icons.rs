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
        icons!(ctx, back, forward, up, recent, refresh, view, rotate_left, rotate_right, rename, delete, explorer, more, folder, star)
    }
}

pub const ICON: f32 = 30.0;
const H: f32 = 56.0;

/// Toolbar button: icon on top, label below. Hover lights it up, pressing pushes
/// the icon down a little so the click is felt; disabled ones are greyed out.
pub fn tool_button(ui: &mut Ui, icon: &TextureHandle, label: &str, enabled: bool) -> Response {
    let font = FontId::proportional(12.5);
    let text_w = ui.fonts(|f| f.layout_no_wrap(label.to_owned(), font.clone(), Color32::BLACK).size().x);
    let w = (text_w + 14.0).max(48.0);
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
    let icon_rect = Rect::from_center_size(rect.center_top() + vec2(0.0, 6.0 + ICON / 2.0 + push), vec2(ICON, ICON));
    let tint = if enabled { Color32::WHITE } else { Color32::from_white_alpha(70) };
    p.image(icon.id(), icon_rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), tint);
    let color = if enabled { Color32::from_gray(25) } else { Color32::from_gray(160) };
    p.text(rect.center_bottom() - vec2(0.0, 9.0 - push), Align2::CENTER_CENTER, label, font, color);
    resp
}
