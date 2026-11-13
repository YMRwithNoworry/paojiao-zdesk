//! Windows 11 Fluent design tokens for the Explorer chrome and desktop fences.
//!
//! Values follow the WinUI 3 light/dark resource dictionaries. Semantic roles
//! use ZSUI's [`ColorRole`] so chrome code never hard-codes per-control colors.

pub use zsui::{Color, ColorRole, ZsIcon};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode { Light, Dark }

/// Colors for one theme mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub mode: Mode,
    /// Tab strip when no DWM backdrop is visible.
    pub strip: Color,
    /// Active tab, address row and command bar (the "layer" above Mica).
    pub layer: Color,
    /// Shell content, tree and status bar background.
    pub content: Color,
    pub text: Color,
    pub text_secondary: Color,
    pub text_disabled: Color,
    pub accent: Color,
    pub accent_text: Color,
    /// Subtle button hover / pressed fills.
    pub hover: Color,
    pub pressed: Color,
    /// Text box fill (solid so native EDIT controls can match it).
    pub input: Color,
    pub input_focused: Color,
    pub input_border: Color,
    pub input_underline: Color,
    pub stroke: Color,
    pub divider: Color,
    pub danger: Color,
    pub inactive_tab_separator: Color,
}

impl Palette {
    pub const fn light() -> Self {
        Self {
            mode: Mode::Light,
            strip: Color::rgb(0xEE, 0xF1, 0xF6),
            layer: Color::rgb(0xF9, 0xF9, 0xF9),
            content: Color::rgb(0xFF, 0xFF, 0xFF),
            text: Color::rgba(0, 0, 0, 0xE4),
            text_secondary: Color::rgba(0, 0, 0, 0x9B),
            text_disabled: Color::rgba(0, 0, 0, 0x5C),
            accent: Color::rgb(0x00, 0x5F, 0xB8),
            accent_text: Color::rgb(0xFF, 0xFF, 0xFF),
            hover: Color::rgba(0, 0, 0, 0x0F),
            pressed: Color::rgba(0, 0, 0, 0x09),
            input: Color::rgb(0xFF, 0xFF, 0xFF),
            input_focused: Color::rgb(0xFF, 0xFF, 0xFF),
            input_border: Color::rgba(0, 0, 0, 0x0F),
            input_underline: Color::rgba(0, 0, 0, 0x72),
            stroke: Color::rgba(0, 0, 0, 0x0F),
            divider: Color::rgba(0, 0, 0, 0x14),
            danger: Color::rgb(0xC4, 0x2B, 0x1C),
            inactive_tab_separator: Color::rgba(0, 0, 0, 0x24),
        }
    }

    pub const fn dark() -> Self {
        Self {
            mode: Mode::Dark,
            strip: Color::rgb(0x20, 0x20, 0x20),
            layer: Color::rgb(0x2C, 0x2C, 0x2C),
            content: Color::rgb(0x19, 0x19, 0x19),
            text: Color::rgb(0xFF, 0xFF, 0xFF),
            text_secondary: Color::rgba(0xFF, 0xFF, 0xFF, 0xC8),
            text_disabled: Color::rgba(0xFF, 0xFF, 0xFF, 0x5D),
            accent: Color::rgb(0x60, 0xCD, 0xFF),
            accent_text: Color::rgb(0, 0, 0),
            hover: Color::rgba(0xFF, 0xFF, 0xFF, 0x0F),
            pressed: Color::rgba(0xFF, 0xFF, 0xFF, 0x0A),
            input: Color::rgb(0x37, 0x37, 0x37),
            input_focused: Color::rgb(0x1F, 0x1F, 0x1F),
            input_border: Color::rgba(0xFF, 0xFF, 0xFF, 0x12),
            input_underline: Color::rgba(0xFF, 0xFF, 0xFF, 0x8B),
            stroke: Color::rgba(0xFF, 0xFF, 0xFF, 0x12),
            divider: Color::rgba(0xFF, 0xFF, 0xFF, 0x15),
            danger: Color::rgb(0xC4, 0x2B, 0x1C),
            inactive_tab_separator: Color::rgba(0xFF, 0xFF, 0xFF, 0x28),
        }
    }

    pub fn for_mode(mode: Mode) -> Self {
        let mut palette=match mode { Mode::Light => Self::light(), Mode::Dark => Self::dark() };
        if let Some(native)=high_contrast_palette(mode){
            let role=|value|native.resolve(value);
            palette.strip=role(ColorRole::Surface);palette.layer=role(ColorRole::SurfaceRaised);palette.content=role(ColorRole::Surface);
            palette.text=role(ColorRole::PrimaryText);palette.text_secondary=role(ColorRole::SecondaryText);palette.text_disabled=role(ColorRole::DisabledText);
            palette.accent=role(ColorRole::Accent);palette.accent_text=role(ColorRole::AccentText);
            palette.hover=role(ColorRole::Control);palette.pressed=role(ColorRole::Control);
            palette.input=role(ColorRole::Surface);palette.input_focused=role(ColorRole::Surface);
            palette.input_border=role(ColorRole::Border);palette.input_underline=role(ColorRole::StrongStroke);
            palette.stroke=role(ColorRole::Border);palette.divider=role(ColorRole::Border);palette.danger=role(ColorRole::Danger);
            palette.inactive_tab_separator=role(ColorRole::StrongStroke);
        }
        palette
    }

    /// Uses the user's accent color (already shaded for this mode) for fills,
    /// focus and selection, with readable text on top of it.
    pub fn with_accent(mut self, [r, g, b]: [u8; 3]) -> Self {
        self.accent = Color::rgb(r, g, b);
        self.accent_text = if luminance(r, g, b) > 0.45 { Color::rgb(0, 0, 0) } else { Color::rgb(0xFF, 0xFF, 0xFF) };
        self
    }

    /// Resolves a ZSUI semantic role.
    pub const fn role(&self, role: ColorRole) -> Color {
        match role {
            ColorRole::PrimaryText => self.text,
            ColorRole::SecondaryText => self.text_secondary,
            ColorRole::DisabledText => self.text_disabled,
            ColorRole::Accent => self.accent,
            ColorRole::AccentText => self.accent_text,
            ColorRole::Surface => self.content,
            ColorRole::SurfaceRaised => self.layer,
            ColorRole::Control => self.input,
            ColorRole::Border => self.stroke,
            ColorRole::StrongStroke => self.input_underline,
            ColorRole::Success => Color::rgb(0x0F, 0x7B, 0x0F),
            ColorRole::Warning => Color::rgb(0x9D, 0x5D, 0x00),
            ColorRole::Danger => self.danger,
        }
    }
}

/// Desktop fence appearance (translucent layer over the wallpaper).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FencePalette {
    pub background: Color,
    pub title: Color,
    pub border: Color,
    pub text: Color,
    pub text_secondary: Color,
    pub text_shadow: Color,
    pub hover: Color,
    /// Active tab chip in a tabbed fence title bar.
    pub chip: Color,
    pub selected: Color,
    pub selected_border: Color,
    pub accent: Color,
}

/// Relative luminance (sRGB approximation) in 0..=1.
fn luminance(r: u8, g: u8, b: u8) -> f32 {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

impl FencePalette {
    /// Selection and drop highlights follow the user's accent color.
    pub fn with_accent(mut self, [r, g, b]: [u8; 3]) -> Self {
        self.accent = Color::rgb(r, g, b);
        self.selected = Color::rgba(r, g, b, self.selected.a);
        self.selected_border = Color::rgba(r, g, b, self.selected_border.a);
        self
    }

    /// `opacity` is the background opacity in percent (10–100).
    pub fn new(mode: Mode, opacity: u8) -> Self {
        if let Some(native)=high_contrast_palette(mode){
            let role=|value|native.resolve(value);
            return Self{background:role(ColorRole::Surface),title:role(ColorRole::SurfaceRaised),border:role(ColorRole::StrongStroke),
                text:role(ColorRole::PrimaryText),text_secondary:role(ColorRole::SecondaryText),text_shadow:Color::rgba(0,0,0,0),
                hover:role(ColorRole::Control),chip:role(ColorRole::Control),selected:role(ColorRole::Surface),
                selected_border:role(ColorRole::Accent),accent:role(ColorRole::Accent)};
        }
        Self::configured(mode,opacity)
    }
    fn configured(mode:Mode,opacity:u8)->Self{
        let a = (opacity.clamp(10, 100) as u32 * 255 / 100) as u8;
        match mode {
            Mode::Dark => Self {
                background: Color::rgba(0x20, 0x22, 0x27, a),
                title: Color::rgba(0x14, 0x16, 0x1A, (a as u32 * 2 / 5) as u8),
                border: Color::rgba(0xFF, 0xFF, 0xFF, 0x1C),
                text: Color::rgb(0xF5, 0xF6, 0xF8),
                text_secondary: Color::rgba(0xFF, 0xFF, 0xFF, 0xB0),
                text_shadow: Color::rgba(0, 0, 0, 0xA0),
                hover: Color::rgba(0xFF, 0xFF, 0xFF, 0x18),
                chip: Color::rgba(0xFF, 0xFF, 0xFF, 0x29),
                selected: Color::rgba(0x60, 0xCD, 0xFF, 0x40),
                selected_border: Color::rgba(0x60, 0xCD, 0xFF, 0x90),
                accent: Color::rgb(0x60, 0xCD, 0xFF),
            },
            Mode::Light => Self {
                background: Color::rgba(0xF7, 0xF9, 0xFC, a),
                title: Color::rgba(0xFF, 0xFF, 0xFF, (a as u32 / 5) as u8),
                border: Color::rgba(0, 0, 0, 0x1A),
                text: Color::rgba(0, 0, 0, 0xE4),
                text_secondary: Color::rgba(0, 0, 0, 0x9B),
                text_shadow: Color::rgba(0xFF, 0xFF, 0xFF, 0x00),
                hover: Color::rgba(0, 0, 0, 0x0D),
                chip: Color::rgba(0, 0, 0, 0x14),
                selected: Color::rgba(0x00, 0x5F, 0xB8, 0x30),
                selected_border: Color::rgba(0x00, 0x5F, 0xB8, 0x80),
                accent: Color::rgb(0x00, 0x5F, 0xB8),
            },
        }
    }
}

fn high_contrast_palette(mode:Mode)->Option<zsui::PlatformPalette>{
    let mode=match mode{Mode::Light=>zsui::ZsuiThemeMode::Light,Mode::Dark=>zsui::ZsuiThemeMode::Dark};
    zsui::PlatformPalette::current(mode).ok().filter(|palette|palette.mode==zsui::ZsuiThemeMode::HighContrast)
}

/// Segoe Fluent Icons / Segoe MDL2 Assets code points. Semantic icons come
/// from ZSUI's [`ZsIcon`]; Explorer-specific glyphs are listed here.
pub mod glyph {
    use super::ZsIcon;
    pub const BACK: &str = "\u{E72B}";
    pub const FORWARD: &str = "\u{E72A}";
    pub const UP: &str = "\u{E74A}";
    pub const REFRESH: &str = ZsIcon::Refresh.windows_fluent_glyph();
    pub const ADD: &str = ZsIcon::Add.windows_fluent_glyph();
    pub const CLOSE_SMALL: &str = "\u{E711}";
    pub const SEARCH: &str = ZsIcon::Search.windows_fluent_glyph();
    pub const CHEVRON_RIGHT: &str = "\u{E76C}";
    pub const CHEVRON_DOWN: &str = "\u{E70D}";
    pub const CHEVRON_DOWN_SMALL: &str = "\u{E96E}";
    pub const CUT: &str = ZsIcon::Cut.windows_fluent_glyph();
    pub const COPY: &str = ZsIcon::Copy.windows_fluent_glyph();
    pub const PASTE: &str = ZsIcon::Paste.windows_fluent_glyph();
    pub const RENAME: &str = "\u{E8AC}";
    pub const DELETE: &str = ZsIcon::Delete.windows_fluent_glyph();
    pub const SHARE: &str = "\u{E72D}";
    pub const SORT: &str = "\u{E8CB}";
    pub const VIEW: &str = "\u{E8A9}";
    pub const MORE: &str = ZsIcon::More.windows_fluent_glyph();
    pub const MENU: &str = "\u{E700}";
    pub const STAR: &str = "\u{E734}";
    pub const STAR_FILLED: &str = "\u{E735}";
    pub const DETAILS_VIEW: &str = "\u{E8FD}";
    pub const TILES_VIEW: &str = "\u{E8B9}";
    /// Two pages side by side (distinct from the View command's grid).
    pub const DUAL_PANE: &str = "\u{E89A}";
    pub const PROPERTIES: &str = "\u{E946}";
    pub const PREVIEW: &str = "\u{E8FF}";
    pub const OVERFLOW: &str = "\u{E76C}\u{E76C}";
    pub const TABS: &str = "\u{E737}";
    pub const LOCK: &str = "\u{E72E}";
    pub const PIN: &str = ZsIcon::Pin.windows_fluent_glyph();
    pub const FOLDER: &str = ZsIcon::Folder.windows_fluent_glyph();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn palettes_keep_readable_contrast() {
        for p in [Palette::light(), Palette::dark()] {
            assert_ne!(p.text, p.layer);
            assert_eq!(p.role(ColorRole::SurfaceRaised), p.layer);
            assert_eq!(p.input.a, 255, "native EDIT backgrounds must be opaque");
            assert_eq!(p.input_focused.a, 255);
        }
    }
    #[test]
    fn custom_accent_keeps_readable_text_and_selection_alpha() {
        let light = Palette::light().with_accent([0x00, 0x67, 0xC0]);
        assert_eq!(light.accent_text, Color::rgb(0xFF, 0xFF, 0xFF));
        let dark = Palette::dark().with_accent([0x4C, 0xC2, 0xFF]);
        assert_eq!(dark.accent_text, Color::rgb(0, 0, 0));
        let fence = FencePalette::configured(Mode::Light, 80);
        let tinted = fence.with_accent([0xE7, 0x48, 0x56]);
        assert_eq!((tinted.selected.r, tinted.selected.a), (0xE7, fence.selected.a));
        assert_eq!(tinted.selected_border.a, fence.selected_border.a);
    }
    #[test]
    fn fence_opacity_is_clamped() {
        assert_eq!(FencePalette::configured(Mode::Dark, 0).background.a, 25);
        assert_eq!(FencePalette::configured(Mode::Dark, 200).background.a, 255);
    }
    #[test]
    fn zsui_glyphs_are_private_use_code_points() {
        for g in [glyph::ADD, glyph::REFRESH, glyph::SEARCH, glyph::CUT, glyph::DELETE] {
            let c = g.chars().next().unwrap() as u32;
            assert!((0xE000..=0xF8FF).contains(&c));
        }
    }
}
