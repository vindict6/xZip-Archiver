//! Look and feel: palette, accent, typography, and a few painting helpers
//! (gradients, rings, glows) that egui does not ship but a modern app needs.

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Pos2, Rect, Stroke, Visuals,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Accent {
    Cyan,
    Violet,
    Emerald,
    Amber,
    Rose,
}

impl Accent {
    pub const ALL: [Accent; 5] = [
        Accent::Cyan,
        Accent::Violet,
        Accent::Emerald,
        Accent::Amber,
        Accent::Rose,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Accent::Cyan => "Cyan",
            Accent::Violet => "Violet",
            Accent::Emerald => "Emerald",
            Accent::Amber => "Amber",
            Accent::Rose => "Rose",
        }
    }

    pub fn from_name(s: &str) -> Accent {
        Accent::ALL
            .into_iter()
            .find(|a| a.name() == s)
            .unwrap_or(Accent::Cyan)
    }

    /// (main colour, second colour of the gradient)
    pub fn colors(self, dark: bool) -> (Color32, Color32) {
        match (self, dark) {
            (Accent::Cyan, true) => (
                Color32::from_rgb(79, 195, 247),
                Color32::from_rgb(124, 110, 255),
            ),
            (Accent::Cyan, false) => (
                Color32::from_rgb(0, 122, 204),
                Color32::from_rgb(98, 70, 234),
            ),
            (Accent::Violet, true) => (
                Color32::from_rgb(167, 139, 250),
                Color32::from_rgb(236, 72, 153),
            ),
            (Accent::Violet, false) => (
                Color32::from_rgb(109, 40, 217),
                Color32::from_rgb(219, 39, 119),
            ),
            (Accent::Emerald, true) => (
                Color32::from_rgb(52, 211, 153),
                Color32::from_rgb(34, 211, 238),
            ),
            (Accent::Emerald, false) => (
                Color32::from_rgb(5, 150, 105),
                Color32::from_rgb(8, 145, 178),
            ),
            (Accent::Amber, true) => (
                Color32::from_rgb(251, 191, 36),
                Color32::from_rgb(249, 115, 22),
            ),
            (Accent::Amber, false) => (
                Color32::from_rgb(217, 119, 6),
                Color32::from_rgb(234, 88, 12),
            ),
            (Accent::Rose, true) => (
                Color32::from_rgb(251, 113, 133),
                Color32::from_rgb(192, 132, 252),
            ),
            (Accent::Rose, false) => (
                Color32::from_rgb(225, 29, 72),
                Color32::from_rgb(147, 51, 234),
            ),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub bg2: Color32,
    pub panel: Color32,
    pub card: Color32,
    pub card_hover: Color32,
    pub stripe: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub accent2: Color32,
    pub accent_soft: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub border: Color32,
    pub dark: bool,
}

pub fn palette(dark: bool, accent: Accent) -> Palette {
    let (a, a2) = accent.colors(dark);
    if dark {
        Palette {
            bg: Color32::from_rgb(14, 17, 25),
            bg2: Color32::from_rgb(22, 26, 40),
            panel: Color32::from_rgb(20, 24, 34),
            card: Color32::from_rgb(28, 33, 46),
            card_hover: Color32::from_rgb(36, 42, 58),
            stripe: Color32::from_rgb(24, 29, 40),
            text: Color32::from_rgb(232, 236, 243),
            text_dim: Color32::from_rgb(140, 150, 170),
            accent: a,
            accent2: a2,
            accent_soft: mix(a, Color32::from_rgb(20, 24, 34), 0.78),
            ok: Color32::from_rgb(96, 211, 148),
            warn: Color32::from_rgb(245, 189, 78),
            danger: Color32::from_rgb(244, 97, 97),
            border: Color32::from_rgb(44, 51, 68),
            dark,
        }
    } else {
        Palette {
            bg: Color32::from_rgb(243, 245, 250),
            bg2: Color32::from_rgb(232, 236, 246),
            panel: Color32::from_rgb(255, 255, 255),
            card: Color32::from_rgb(255, 255, 255),
            card_hover: Color32::from_rgb(238, 242, 250),
            stripe: Color32::from_rgb(247, 249, 253),
            text: Color32::from_rgb(26, 30, 40),
            text_dim: Color32::from_rgb(108, 118, 138),
            accent: a,
            accent2: a2,
            accent_soft: mix(a, Color32::WHITE, 0.82),
            ok: Color32::from_rgb(28, 150, 90),
            warn: Color32::from_rgb(200, 135, 15),
            danger: Color32::from_rgb(210, 55, 55),
            border: Color32::from_rgb(220, 226, 236),
            dark,
        }
    }
}

/// t = 0 gives `a`, t = 1 gives `b`.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

pub fn with_alpha(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

pub fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "inter".into(),
        FontData::from_static(include_bytes!("../../../assets/Inter-Regular.ttf")).into(),
    );
    fonts.font_data.insert(
        "inter-semibold".into(),
        FontData::from_static(include_bytes!("../../../assets/Inter-SemiBold.ttf")).into(),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "inter".into());
    fonts
        .families
        .insert(semibold(), vec!["inter-semibold".into(), "inter".into()]);
    // Phosphor goes last as a fallback. The bundled Inter files have their private-use
    // glyphs stripped (fonttools), otherwise they would shadow some of the icons.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    let icons: Vec<String> = fonts
        .families
        .get(&FontFamily::Proportional)
        .map(|f| {
            f.iter()
                .filter(|n| n.contains("phosphor"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    fonts.families.entry(semibold()).or_default().extend(icons);
    fonts
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = p.panel;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.stripe;
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_shadow.color = Color32::from_black_alpha(if p.dark { 140 } else { 40 });
    v.popup_shadow.color = Color32::from_black_alpha(if p.dark { 120 } else { 30 });
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.override_text_color = Some(p.text);
    let r = CornerRadius::same(9);
    v.widgets.noninteractive.bg_fill = p.card;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.bg_fill = p.card;
    v.widgets.inactive.weak_bg_fill = p.card;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.bg_fill = p.card_hover;
    v.widgets.hovered.weak_bg_fill = p.card_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, with_alpha(p.accent, 180));
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.bg_fill = p.accent_soft;
    v.widgets.active.weak_bg_fill = p.accent_soft;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.corner_radius = r;
    v.widgets.open.bg_fill = p.card_hover;
    v.widgets.open.corner_radius = r;
    v.text_cursor.stroke.color = p.accent;
    let theme = if p.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_theme(theme);
    ctx.set_visuals_of(theme, v);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.window_margin = egui::Margin::same(18);
        style.spacing.menu_margin = egui::Margin::same(8);
        style.spacing.interact_size.y = 30.0;
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.floating = true;
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(14.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(14.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Heading,
            egui::FontId::new(20.0, semibold()),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(11.5, FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(13.0, FontFamily::Monospace),
        );
    });
}

// ---- painting helpers ---------------------------------------------------------------

/// Vertical gradient fill.
pub fn gradient_rect(painter: &egui::Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

/// Horizontal gradient fill (progress bars, the logo tile).
pub fn gradient_rect_h(painter: &egui::Painter, rect: Rect, left: Color32, right: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), left);
    mesh.colored_vertex(rect.right_top(), right);
    mesh.colored_vertex(rect.right_bottom(), right);
    mesh.colored_vertex(rect.left_bottom(), left);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

/// A soft radial glow: concentric circles fading out.
pub fn glow(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let steps = 8;
    for i in (1..=steps).rev() {
        let t = i as f32 / steps as f32;
        painter.circle_filled(
            center,
            radius * t,
            with_alpha(color, (28.0 * (1.0 - t) + 6.0) as u8),
        );
    }
}

/// Arc from 12 o'clock, clockwise, `fraction` of a full turn, over a faint track.
pub fn ring(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    width: f32,
    fraction: f32,
    color: Color32,
    track: Color32,
) {
    painter.circle_stroke(center, radius, Stroke::new(width, track));
    let f = fraction.clamp(0.0, 1.0);
    if f <= 0.0 {
        return;
    }
    let steps = (f * 64.0).ceil().max(2.0) as usize;
    let pts: Vec<Pos2> = (0..=steps)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2
                + f * std::f32::consts::TAU * (i as f32 / steps as f32);
            Pos2::new(center.x + radius * a.cos(), center.y + radius * a.sin())
        })
        .collect();
    painter.add(egui::Shape::line(pts, Stroke::new(width, color)));
}
