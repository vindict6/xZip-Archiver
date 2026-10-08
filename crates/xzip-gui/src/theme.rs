//! Colours and spacing. One accent, generous rounding, soft contrast; the kind of
//! surface a file manager should have rather than a 1998 dialog.

use egui::{Color32, CornerRadius, Stroke, Visuals};

pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub card: Color32,
    pub card_hover: Color32,
    pub stripe: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub border: Color32,
}

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(17, 20, 28),
    panel: Color32::from_rgb(23, 27, 37),
    card: Color32::from_rgb(30, 35, 48),
    card_hover: Color32::from_rgb(38, 44, 60),
    stripe: Color32::from_rgb(26, 31, 42),
    text: Color32::from_rgb(230, 234, 240),
    text_dim: Color32::from_rgb(140, 150, 168),
    accent: Color32::from_rgb(79, 195, 247),
    accent_soft: Color32::from_rgb(28, 62, 84),
    ok: Color32::from_rgb(96, 211, 148),
    warn: Color32::from_rgb(245, 189, 78),
    danger: Color32::from_rgb(244, 97, 97),
    border: Color32::from_rgb(44, 51, 68),
};

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(244, 246, 250),
    panel: Color32::from_rgb(255, 255, 255),
    card: Color32::from_rgb(255, 255, 255),
    card_hover: Color32::from_rgb(236, 241, 248),
    stripe: Color32::from_rgb(248, 250, 253),
    text: Color32::from_rgb(28, 32, 42),
    text_dim: Color32::from_rgb(110, 120, 138),
    accent: Color32::from_rgb(0, 122, 204),
    accent_soft: Color32::from_rgb(214, 234, 250),
    ok: Color32::from_rgb(34, 160, 94),
    warn: Color32::from_rgb(205, 140, 20),
    danger: Color32::from_rgb(212, 60, 60),
    border: Color32::from_rgb(220, 226, 236),
};

pub fn apply(ctx: &egui::Context, dark: bool) {
    let p = if dark { &DARK } else { &LIGHT };
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = p.panel;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.stripe;
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_shadow.color = Color32::from_black_alpha(if dark { 120 } else { 40 });
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.override_text_color = Some(p.text);
    let r = CornerRadius::same(8);
    v.widgets.noninteractive.bg_fill = p.card;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.bg_fill = p.card;
    v.widgets.inactive.weak_bg_fill = p.card;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.bg_fill = p.card_hover;
    v.widgets.hovered.weak_bg_fill = p.card_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.bg_fill = p.accent_soft;
    v.widgets.active.weak_bg_fill = p.accent_soft;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.corner_radius = r;
    v.widgets.open.bg_fill = p.card_hover;
    v.widgets.open.corner_radius = r;
    let theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_theme(theme);
    ctx.set_visuals_of(theme, v);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.window_margin = egui::Margin::same(16);
        style.spacing.menu_margin = egui::Margin::same(8);
        style.spacing.interact_size.y = 28.0;
    });
}

pub fn palette(dark: bool) -> &'static Palette {
    if dark {
        &DARK
    } else {
        &LIGHT
    }
}
