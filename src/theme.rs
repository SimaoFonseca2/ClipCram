//! Colors and widget styling. The one strong color is the amber used for the trim
//! region and in/out handles, borrowed from the in/out marks of video editors.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Theme, Visuals};

#[derive(Clone, Copy)]
pub struct Palette {
    pub base: Color32,
    pub surface: Color32,
    pub input: Color32,
    pub button: Color32,
    pub button_hover: Color32,
    pub button_active: Color32,
    pub stroke: Color32,
    pub stroke_strong: Color32,
    pub text: Color32,
    pub weak: Color32,
    /// Fill color for the trim region, handles and the main button.
    pub accent: Color32,
    /// Accent when used for text or thin lines, tuned for contrast on the surface.
    pub accent_ink: Color32,
    pub on_accent: Color32,
    pub ok: Color32,
    pub bad: Color32,
    pub video_bg: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            base: hex(0x1B1E24),
            surface: hex(0x22262E),
            input: hex(0x15171C),
            button: hex(0x2C313B),
            button_hover: hex(0x353B47),
            button_active: hex(0x3E4553),
            stroke: hex(0x313743),
            stroke_strong: hex(0x495163),
            text: hex(0xE4E7EC),
            weak: hex(0x9AA1AD),
            accent: hex(0xF0A83A),
            accent_ink: hex(0xF3B556),
            on_accent: hex(0x1B1E24),
            ok: hex(0x63C792),
            bad: hex(0xEB6F62),
            video_bg: hex(0x0E0F12),
        }
    } else {
        Palette {
            base: hex(0xECEEF1),
            surface: hex(0xFFFFFF),
            input: hex(0xF5F6F8),
            button: hex(0xE3E6EA),
            button_hover: hex(0xD8DCE2),
            button_active: hex(0xCBD0D8),
            stroke: hex(0xD3D7DE),
            stroke_strong: hex(0xAEB5C0),
            text: hex(0x1C2027),
            weak: hex(0x59616E),
            accent: hex(0xE89E2E),
            accent_ink: hex(0x8F5300),
            on_accent: hex(0x1C2027),
            ok: hex(0x237A4D),
            bad: hex(0xB93C30),
            video_bg: hex(0x0E0F12),
        }
    }
}

pub fn current(ctx: &egui::Context) -> Palette {
    palette(ctx.style().visuals.dark_mode)
}

fn visuals(dark: bool) -> Visuals {
    let p = palette(dark);
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let radius = CornerRadius::same(6);

    v.panel_fill = p.base;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.stroke);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.extreme_bg_color = p.input;
    v.text_edit_bg_color = Some(p.input);
    v.faint_bg_color = p.surface;
    v.weak_text_color = Some(p.weak);
    v.hyperlink_color = p.accent_ink;
    v.warn_fg_color = p.accent_ink;
    v.error_fg_color = p.bad;
    v.selection.bg_fill = if dark {
        Color32::from_rgba_unmultiplied(240, 168, 58, 70)
    } else {
        Color32::from_rgba_unmultiplied(232, 158, 46, 90)
    };
    v.selection.stroke = Stroke::new(1.0, p.accent_ink);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.stroke);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = radius;

    w.inactive.bg_fill = p.input;
    w.inactive.weak_bg_fill = p.button;
    w.inactive.bg_stroke = Stroke::new(1.0, p.stroke);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = radius;

    w.hovered.bg_fill = p.button_hover;
    w.hovered.weak_bg_fill = p.button_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.stroke_strong);
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = radius;

    w.active.bg_fill = p.button_active;
    w.active.weak_bg_fill = p.button_active;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.corner_radius = radius;

    w.open.bg_fill = p.button_hover;
    w.open.weak_bg_fill = p.button_hover;
    w.open.bg_stroke = Stroke::new(1.0, p.stroke_strong);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = radius;

    v
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals_of(Theme::Dark, visuals(true));
    ctx.set_visuals_of(Theme::Light, visuals(false));

    for theme in [Theme::Dark, Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style.text_styles = [
                (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
                (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
                (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
                (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
            ]
            .into();
            style.spacing.item_spacing = egui::vec2(8.0, 7.0);
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
            style.spacing.interact_size.y = 26.0;
            style.spacing.combo_width = 220.0;
        });
    }
}
