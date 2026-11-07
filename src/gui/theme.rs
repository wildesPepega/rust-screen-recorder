use egui::{Color32, CornerRadius, Stroke, Visuals};

pub const ACCENT: Color32 = Color32::from_rgb(0x3d, 0xdc, 0x97);
pub const RED: Color32 = Color32::from_rgb(0xff, 0x4d, 0x4f);
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x18, 0x1d);
pub const PANEL_2: Color32 = Color32::from_rgb(0x20, 0x23, 0x2a);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x9a, 0xa0, 0xab);

pub fn apply(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.window_fill = PANEL;
    v.panel_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(0x0f, 0x10, 0x14);
    v.faint_bg_color = PANEL_2;
    v.selection.bg_fill = ACCENT.linear_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    v.hyperlink_color = ACCENT;
    let r = CornerRadius::same(6);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.corner_radius = r;
    v.widgets.open.corner_radius = r;
    v.widgets.inactive.weak_bg_fill = PANEL_2;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT.linear_multiply(0.7));
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
    });
}
