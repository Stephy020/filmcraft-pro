//! The EpsiCut wordmark: the app icon followed by the name, drawn in code. The icon is decoded
//! once and kept as a texture.

const ICON_PNG: &[u8] = include_bytes!("../../../assets/app-icon/hicolor/128x128/apps/ai.storyteller.epsicut.png");

/// The app name as the wordmark spells it.
pub const NAME: &str = "EpsiCut";

/// The app icon as a texture (`None` when it cannot be decoded).
pub fn icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let id = egui::Id::unique("epsicut-icon");
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(t);
    }
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).ok()?.to_rgba8();
    let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
    let tex = ctx.load_texture("epsicut-icon", color, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    Some(tex)
}

/// Draw the wordmark `height` points tall with its left edge at `left_center` (vertically centred):
/// the icon, then the name in light ink on dark themes (`dark`) and dark ink on light ones.
/// Returns the drawn rect.
pub fn paint_wordmark(ui: &egui::Ui, left_center: egui::Pos2, height: f32, dark: bool) -> Option<egui::Rect> {
    let height = if height.is_finite() { height.clamp(4.0, 512.0) } else { 16.0 };
    let painter = ui.painter();
    let top = left_center.y - height / 2.0;
    let mut right = left_center.x;
    if let Some(tex) = icon(ui.ctx()) {
        let r = egui::Rect::from_min_size(egui::pos2(left_center.x, top), egui::vec2(height, height));
        painter.image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        right = r.max.x + height * 0.3;
    }
    let ink = if dark { egui::Color32::from_rgb(0xf4, 0xf4, 0xf8) } else { egui::Color32::from_rgb(0x15, 0x1a, 0x3a) };
    let g = painter.layout_no_wrap(NAME.to_string(), crate::theme::Tokens::semibold(height * 0.8), ink);
    let size = g.size();
    painter.galley(egui::pos2(right, left_center.y - size.y / 2.0), g, ink);
    Some(egui::Rect::from_min_max(egui::pos2(left_center.x, top), egui::pos2(right + size.x, top + height)))
}
