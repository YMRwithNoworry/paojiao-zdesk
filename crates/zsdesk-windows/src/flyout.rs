//! Windows 11 style popup menus for application commands.
//!
//! `TrackPopupMenu` keeps the classic menu look: square on Windows 10 and only
//! rounded on Windows 11, with the old check gutter and dense rows. Application
//! menus (≡, "…", view, sort, tabs, fences) are drawn here instead with the
//! WinUI `MenuFlyout` design on both systems: an 8-DIP rounded body with a
//! soft shadow, the flyout surface colors of the current theme, rounded hover
//! plates, hairline separators, check marks, accelerators and submenu
//! chevrons. Windows Shell (file) context menus keep their native presenter.
//!
//! [`track`] is modal like `TrackPopupMenu`: it returns the chosen command
//! while keeping the caller's thread pumping non-input messages.

use std::{cell::{Cell, RefCell}, rc::Rc, time::{Duration, Instant}};

use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM},
        Graphics::Gdi::{GetDC, GetMonitorInfoW, MonitorFromPoint, ReleaseDC, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, MONITORINFO, MONITOR_DEFAULTTONEAREST},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_APPS, VK_DOWN, VK_END, VK_ESCAPE,
                VK_F10, VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_RETURN, VK_RIGHT, VK_RWIN, VK_SPACE, VK_TAB, VK_UP},
            WindowsAndMessaging::*,
        },
    },
};

use crate::flyout_layout::{self as geometry, Anchor, Bounds, LevelLayout, Measured, Metrics, RowKind};
use crate::gfx::{Align, Canvas, Color, Gfx, RectF, Surface, TextStyle};

/// A Fluent glyph shown in the icon column; `accent` draws it in the accent color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyph { pub code: &'static str, pub accent: bool }

/// One menu entry. Labels may contain `&` mnemonics and a `\t` accelerator.
#[derive(Clone, Debug)]
pub enum FlyoutItem {
    Command { id: usize, label: String, enabled: bool, checked: bool, glyph: Option<Glyph> },
    Separator,
    Submenu { label: String, enabled: bool, glyph: Option<Glyph>, items: Vec<FlyoutItem> },
}

impl FlyoutItem {
    fn kind(&self) -> RowKind { if matches!(self, Self::Separator) { RowKind::Separator } else { RowKind::Item } }
    fn enabled(&self) -> bool { match self { Self::Command { enabled, .. } | Self::Submenu { enabled, .. } => *enabled, Self::Separator => false } }
    fn label(&self) -> &str { match self { Self::Command { label, .. } | Self::Submenu { label, .. } => label, Self::Separator => "" } }
    fn glyph(&self) -> Option<Glyph> { match self { Self::Command { glyph, .. } | Self::Submenu { glyph, .. } => *glyph, Self::Separator => None } }
    fn checked(&self) -> bool { matches!(self, Self::Command { checked: true, .. }) }
}

const CLASS: windows::core::PCWSTR = w!("ZSDesk.Win11Flyout");
const TIMER: usize = 0x5A46;
const HOVER_DELAY: Duration = Duration::from_millis(300);
const CHECK: &str = "\u{E73E}";
const CHEVRON: &str = "\u{E76C}";

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static CANCEL: Cell<bool> = const { Cell::new(false) };
    static GRAPHICS: RefCell<Option<Rc<Gfx>>> = const { RefCell::new(None) };
    static PROFILE: RefCell<Option<Rc<Profile>>> = const { RefCell::new(None) };
}

/// What menus need from the system, detected once instead of on every
/// opening: both theme palettes with the user's accent color. Refreshed by
/// [`refresh_system`] when the theme, accent or contrast settings change.
struct Profile { light: Palette, dark: Palette }

fn profile() -> Rc<Profile> {
    PROFILE.with(|slot| slot.borrow_mut().get_or_insert_with(|| Rc::new(Profile { light: palette(false), dark: palette(true) })).clone())
}

/// Prepares this UI thread's menus ahead of the first opening: window class,
/// Direct2D/DirectWrite factories and render target, the menu text formats at
/// the primary monitor's DPI and the theme profile.
pub fn prepare() {
    register_class();
    let _ = profile();
    if let Some(gfx) = graphics() {
        let (_, dpi) = monitor_at(POINT::default());
        let metrics = Metrics::new(dpi);
        for size in [geometry::TEXT_DIP, geometry::ACCELERATOR_DIP] { let _ = gfx.measure("菜单 Menu", &TextStyle::ui(metrics.px(size)), 4096.0); }
        let _ = gfx.measure(CHECK, &TextStyle::icon(metrics.px(12.0)), 4096.0);
        if let Some(surface) = Surface::new(1, 1) { let _ = gfx.draw(&surface, |_| {}); }
    }
}

/// Re-reads the theme profile (dark mode, accent color, high contrast) after
/// a system setting change; the next menu uses the new values.
pub fn refresh_system() {
    PROFILE.with(|slot| *slot.borrow_mut() = None);
    GRAPHICS.with(|slot| *slot.borrow_mut() = None);
}

/// Release drawing caches when a reusable STA has finished its file window.
/// Active menus keep their resources until tracking returns; the next window
/// prepares its own cache before accepting input.
pub fn release_thread_resources() {
    if is_open() { return; }
    GRAPHICS.with(|slot| { slot.borrow_mut().take(); });
    PROFILE.with(|slot| { slot.borrow_mut().take(); });
}

/// A flyout menu is being tracked on this thread (used where Win32 menu mode
/// flags were checked, e.g. to keep a rolled-down fence open).
pub fn is_open() -> bool { ACTIVE.with(Cell::get) }

/// Dismisses this thread's open flyout, like `EndMenu` does for Win32 menus.
pub fn cancel() { if is_open() { CANCEL.with(|cancel| cancel.set(true)); } }

struct Palette { background: Color, border: Color, text: Color, secondary: Color, disabled: Color, hover: Color, pressed: Color, divider: Color, accent: Color, shadow: f32 }

fn palette(dark: bool) -> Palette {
    let accent = crate::theme::system_accent(dark).map(|[r, g, b]| Color::rgba8(r, g, b, 0xFF))
        .unwrap_or(if dark { Color::rgba8(0x60, 0xCD, 0xFF, 0xFF) } else { Color::rgba8(0x00, 0x5F, 0xB8, 0xFF) });
    if dark {
        Palette { background: Color::rgba8(0x2C, 0x2C, 0x2C, 0xFF), border: Color::rgba8(0xFF, 0xFF, 0xFF, 0x17),
            text: Color::rgba8(0xFF, 0xFF, 0xFF, 0xFF), secondary: Color::rgba8(0xFF, 0xFF, 0xFF, 0xC5), disabled: Color::rgba8(0xFF, 0xFF, 0xFF, 0x5D),
            hover: Color::rgba8(0xFF, 0xFF, 0xFF, 0x0F), pressed: Color::rgba8(0xFF, 0xFF, 0xFF, 0x0A), divider: Color::rgba8(0xFF, 0xFF, 0xFF, 0x15),
            accent, shadow: 0.34 }
    } else {
        Palette { background: Color::rgba8(0xF9, 0xF9, 0xF9, 0xFF), border: Color::rgba8(0, 0, 0, 0x14),
            text: Color::rgba8(0, 0, 0, 0xE4), secondary: Color::rgba8(0, 0, 0, 0x9E), disabled: Color::rgba8(0, 0, 0, 0x5C),
            hover: Color::rgba8(0, 0, 0, 0x0A), pressed: Color::rgba8(0, 0, 0, 0x06), divider: Color::rgba8(0, 0, 0, 0x14),
            accent, shadow: 0.16 }
    }
}

struct Text { label: String, mnemonic: Option<char>, accelerator: String }

struct Level<'a> {
    items: &'a [FlyoutItem],
    texts: Vec<Text>,
    hwnd: HWND,
    body: Bounds,
    layout: LevelLayout,
    metrics: Metrics,
    scroll: f32,
    highlight: Option<usize>,
    pressed: bool,
    /// Item of this level whose submenu is the next level.
    open_child: Option<usize>,
    surface: Option<Surface>,
}

impl Level<'_> {
    fn max_scroll(&self) -> f32 { (self.layout.content_height - self.body.height() as f32).max(0.0) }
    fn row_at(&self, point: POINT) -> Option<usize> {
        if !self.body.contains(point.x, point.y) { return None; }
        self.layout.row_at((point.y - self.body.top) as f32, self.scroll, self.metrics)
            .filter(|row| self.items[*row].kind() == RowKind::Item)
    }
    fn selectable(&self) -> Vec<bool> { self.items.iter().map(FlyoutItem::enabled).collect() }
    fn ensure_visible(&mut self, row: usize) {
        let Some(r) = self.layout.rows.get(row) else { return };
        let pad = self.metrics.px(geometry::PADDING_DIP);
        let height = self.body.height() as f32;
        if r.y - self.scroll < pad { self.scroll = (r.y - pad).max(0.0); }
        if r.y + r.h - self.scroll > height - pad { self.scroll = (r.y + r.h - height + pad).min(self.max_scroll()); }
    }
}

fn graphics() -> Option<Rc<Gfx>> {
    GRAPHICS.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() { *slot = Gfx::new(&crate::gfx::system_ui_font(None).0).ok().map(Rc::new); }
        slot.clone()
    })
}

fn register_class() -> bool {
    static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REGISTERED.get_or_init(|| unsafe {
        let Ok(instance) = GetModuleHandleW(None) else { return false };
        let class = WNDCLASSW { lpfnWndProc: Some(flyout_proc), hInstance: instance.into(), lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(), ..Default::default() };
        RegisterClassW(&class) != 0
    })
}

unsafe extern "system" fn flyout_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // Another window took the mouse (a dialog, Alt+Tab, a drag source):
        // the menu closes like a Win32 menu does.
        WM_CAPTURECHANGED | WM_CANCELMODE => { cancel(); unsafe { DefWindowProcW(hwnd, msg, wp, lp) } }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Work area and effective DPI of the monitor containing `point`.
fn monitor_at(point: POINT) -> (Bounds, u32) {
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let work = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        Bounds::new(info.rcWork.left, info.rcWork.top, info.rcWork.right, info.rcWork.bottom)
    } else {
        Bounds::new(0, 0, unsafe { GetSystemMetrics(SM_CXSCREEN) }, unsafe { GetSystemMetrics(SM_CYSCREEN) })
    };
    let (mut x, mut y) = (96u32, 96u32);
    let dpi = if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) }.is_ok() { x.max(96) } else { 96 };
    (work, dpi)
}

enum Place { Root(Anchor), Beside { parent: Bounds, item_top: i32 } }

fn open_level<'a>(gfx: &Gfx, owner: HWND, items: &'a [FlyoutItem], place: Place) -> Option<Level<'a>> {
    if items.is_empty() || !register_class() { return None; }
    let reference = match place {
        Place::Root(Anchor::Point { x, y }) => POINT { x, y },
        Place::Root(Anchor::Below(button)) => POINT { x: button.left, y: button.bottom },
        Place::Beside { parent, item_top } => POINT { x: parent.right, y: item_top },
    };
    let (work, dpi) = monitor_at(reference);
    let metrics = Metrics::new(dpi);
    let text_style = TextStyle::ui(metrics.px(geometry::TEXT_DIP));
    let accelerator_style = TextStyle::ui(metrics.px(geometry::ACCELERATOR_DIP));
    let texts: Vec<Text> = items.iter().map(|item| {
        let (label, mnemonic, accelerator) = geometry::parse_label(item.label());
        Text { label, mnemonic, accelerator }
    }).collect();
    let measured: Vec<Measured> = texts.iter().map(|text| Measured {
        label: gfx.measure(&text.label, &text_style, 4096.0).0,
        accelerator: if text.accelerator.is_empty() { 0.0 } else { gfx.measure(&text.accelerator, &accelerator_style, 4096.0).0 },
    }).collect();
    let kinds: Vec<RowKind> = items.iter().map(FlyoutItem::kind).collect();
    let icon_column = items.iter().any(|item| item.glyph().is_some() || item.checked());
    let has_submenu = items.iter().any(|item| matches!(item, FlyoutItem::Submenu { .. }));
    let layout = geometry::layout(&kinds, &measured, icon_column, has_submenu, metrics);
    let width = layout.width.ceil() as i32;
    let height = geometry::body_height(&layout, work);
    let (x, y) = match place {
        Place::Root(anchor) => geometry::place_root(anchor, width, height, work, metrics),
        Place::Beside { parent, item_top } => geometry::place_submenu(parent, item_top, width, height, work, metrics),
    };
    let body = Bounds::new(x, y, x + width, y + height);
    let shadow = metrics.px(geometry::SHADOW_DIP) as i32;
    let instance = unsafe { GetModuleHandleW(None) }.ok()?;
    let hwnd = unsafe { CreateWindowExW(WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, CLASS, w!(""), WS_POPUP,
        x - shadow, y - shadow, width + shadow * 2, height + shadow * 2, Some(owner), None, Some(instance.into()), None) }.ok()?;
    Some(Level { items, texts, hwnd, body, layout, metrics, scroll: 0.0, highlight: None, pressed: false, open_child: None, surface: None })
}

fn present(level: &mut Level, gfx: &Gfx, colors: &Palette) {
    let shadow = level.metrics.px(geometry::SHADOW_DIP) as i32;
    let (width, height) = (level.body.width() + shadow * 2, level.body.height() + shadow * 2);
    let mut surface = level.surface.take();
    if Surface::ensure(&mut surface, width, height).is_none() { return; }
    let Some(target) = surface.as_ref() else { return };
    let drawn = gfx.draw(target, |canvas| paint(canvas, level, colors));
    if drawn.is_ok() {
        let screen = unsafe { GetDC(None) };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let position = POINT { x: level.body.left - shadow, y: level.body.top - shadow };
        let size = SIZE { cx: width, cy: height };
        let origin = POINT::default();
        unsafe {
            let _ = UpdateLayeredWindow(level.hwnd, Some(screen), Some(&position), Some(&size), Some(target.dc), Some(&origin),
                Default::default(), Some(&blend), ULW_ALPHA);
            ReleaseDC(None, screen);
            if !IsWindowVisible(level.hwnd).as_bool() {
                let _ = SetWindowPos(level.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
        }
    }
    level.surface = surface;
}

fn paint(cv: &Canvas, level: &Level, colors: &Palette) {
    let m = level.metrics;
    let shadow = m.px(geometry::SHADOW_DIP);
    let body = RectF::new(shadow, shadow, level.body.width() as f32, level.body.height() as f32);
    let radius = m.px(geometry::RADIUS_DIP);
    // Soft drop shadow: concentric rounded layers whose alpha sums to the
    // Fluent flyout shadow under the body and fades to zero at the margin.
    let offset = m.px(geometry::SHADOW_OFFSET_DIP);
    let layers = (shadow - offset).max(1.0) as i32;
    let layer_alpha = 1.0 - (1.0 - colors.shadow).powf(1.0 / layers as f32);
    for grow in (1..=layers).rev() {
        let g = grow as f32;
        cv.round(RectF::new(body.x - g, body.y - g + offset, body.w + g * 2.0, body.h + g * 2.0), radius + g, Color::rgba8(0, 0, 0, 0xFF).with_alpha(layer_alpha));
    }
    cv.round(body, radius, colors.background);
    cv.stroke_round(body.inset(0.5, 0.5), (radius - 0.5).max(0.0), colors.border, 1.0);

    cv.push_clip(body.inset(1.0, 1.0));
    let inset = m.px(geometry::INSET_DIP);
    let content_left = m.px(geometry::CONTENT_LEFT_DIP);
    let content_right = m.px(geometry::CONTENT_RIGHT_DIP);
    let chevron_space = if level.layout.has_submenu { m.px(geometry::CHEVRON_DIP) } else { 0.0 };
    let text_style = TextStyle::ui(m.px(geometry::TEXT_DIP));
    let accelerator_style = TextStyle::ui(m.px(geometry::ACCELERATOR_DIP)).align(Align::End);
    let text_left = body.x + level.layout.text_left(m);
    for (index, (row, item)) in level.layout.rows.iter().zip(level.items).enumerate() {
        let y = body.y + row.y - level.scroll;
        if y + row.h < body.y || y > body.bottom() { continue; }
        if row.kind == RowKind::Separator {
            let line = (y + row.h / 2.0).floor() + 0.5;
            cv.line(body.x + 1.0, line, body.right() - 1.0, line, colors.divider, 1.0);
            continue;
        }
        let enabled = item.enabled();
        let plate = RectF::new(body.x + inset, y + m.px(2.0), body.w - inset * 2.0, row.h - m.px(4.0));
        let active = level.highlight == Some(index) || level.open_child == Some(index);
        if active && enabled { cv.round(plate, m.px(4.0), if level.pressed && level.highlight == Some(index) { colors.pressed } else { colors.hover }); }
        let color = if enabled { colors.text } else { colors.disabled };
        let icon = RectF::new(body.x + inset + content_left, y, m.px(geometry::ICON_DIP), row.h);
        if item.checked() {
            cv.glyph(CHECK, icon, m.px(12.0), color);
        } else if let Some(glyph) = item.glyph() {
            let tint = if glyph.accent && enabled { colors.accent } else { color };
            cv.glyph(glyph.code, icon, m.px(geometry::ICON_DIP), tint);
        }
        let right = plate.right() - content_right - chevron_space;
        let text = &level.texts[index];
        let accelerator_width = if text.accelerator.is_empty() { 0.0 } else {
            cv.gfx().measure(&text.accelerator, &accelerator_style, 4096.0).0 + m.px(geometry::ACCELERATOR_GAP_DIP)
        };
        cv.text(&text.label, RectF::new(text_left, y, (right - accelerator_width - text_left).max(0.0), row.h), &text_style, color);
        if !text.accelerator.is_empty() {
            cv.text(&text.accelerator, RectF::new(text_left, y, (right - text_left).max(0.0), row.h), &accelerator_style,
                if enabled { colors.secondary } else { colors.disabled });
        }
        if matches!(item, FlyoutItem::Submenu { .. }) {
            let size = m.px(12.0);
            cv.glyph(CHEVRON, RectF::new(plate.right() - content_right - size, y, size, row.h), m.px(10.0), if enabled { colors.secondary } else { colors.disabled });
        }
    }
    let max_scroll = level.max_scroll();
    if max_scroll > 0.0 {
        let pad = m.px(geometry::PADDING_DIP);
        let track = body.h - pad * 2.0;
        let thumb = (track * body.h / level.layout.content_height).max(m.px(24.0)).min(track);
        let top = body.y + pad + (track - thumb) * level.scroll / max_scroll;
        cv.round(RectF::new(body.right() - m.px(5.0), top, m.px(2.0), thumb), m.px(1.0), colors.secondary.with_alpha(0.5));
    }
    cv.pop_clip();
}

fn close_from(levels: &mut Vec<Level>, depth: usize) {
    while levels.len() > depth {
        if let Some(level) = levels.pop() { let _ = unsafe { DestroyWindow(level.hwnd) }; }
    }
    if let Some(last) = levels.last_mut() { last.open_child = None; }
}

/// Opens the submenu of `row` in level `depth` (closing deeper levels first).
fn open_child<'a>(levels: &mut Vec<Level<'a>>, depth: usize, row: usize, gfx: &Gfx, colors: &Palette, owner: HWND) -> bool {
    close_from(levels, depth + 1);
    let level = &levels[depth];
    let FlyoutItem::Submenu { items, enabled: true, .. } = &level.items[row] else { return false };
    let item_top = level.body.top + (level.layout.rows[row].y - level.scroll) as i32;
    let Some(mut child) = open_level(gfx, owner, items, Place::Beside { parent: level.body, item_top }) else { return false };
    levels[depth].open_child = Some(row);
    present(&mut levels[depth], gfx, colors);
    present(&mut child, gfx, colors);
    levels.push(child);
    true
}

fn set_highlight(level: &mut Level, row: Option<usize>, gfx: &Gfx, colors: &Palette) {
    let row = row.filter(|row| level.items[*row].enabled());
    if level.highlight != row || level.pressed {
        level.highlight = row;
        level.pressed = false;
        present(level, gfx, colors);
    }
}

fn hit(levels: &[Level], point: POINT) -> Option<(usize, Option<usize>)> {
    levels.iter().enumerate().rev().find(|(_, level)| level.body.contains(point.x, point.y)).map(|(depth, level)| (depth, level.row_at(point)))
}

enum Step { Continue, Choose(usize), Dismiss }

const NAVIGATION_KEYS: [VIRTUAL_KEY; 15] = [VK_DOWN, VK_UP, VK_HOME, VK_END, VK_RIGHT, VK_LEFT, VK_ESCAPE, VK_RETURN, VK_SPACE,
    VK_MENU, VK_F10, VK_LWIN, VK_RWIN, VK_APPS, VK_TAB];

/// Command or submenu activation of `row` in the deepest level.
fn activate<'a>(levels: &mut Vec<Level<'a>>, depth: usize, row: usize, gfx: &Gfx, colors: &Palette, owner: HWND, keyboard: bool) -> Step {
    match &levels[depth].items[row] {
        FlyoutItem::Command { id, enabled: true, .. } => Step::Choose(*id),
        FlyoutItem::Submenu { enabled: true, .. } => {
            if open_child(levels, depth, row, gfx, colors, owner) && keyboard {
                let child = levels.len() - 1;
                let first = geometry::next_selectable(&levels[child].selectable(), None, 1);
                set_highlight(&mut levels[child], first, gfx, colors);
            }
            Step::Continue
        }
        _ => Step::Continue,
    }
}

fn key(levels: &mut Vec<Level>, vk: VIRTUAL_KEY, gfx: &Gfx, colors: &Palette, owner: HWND) -> Step {
    let depth = levels.len() - 1;
    match vk {
        VK_DOWN | VK_UP | VK_HOME | VK_END => {
            let level = &mut levels[depth];
            let selectable = level.selectable();
            let next = match vk {
                VK_DOWN => geometry::next_selectable(&selectable, level.highlight, 1),
                VK_UP => geometry::next_selectable(&selectable, level.highlight, -1),
                VK_HOME => geometry::next_selectable(&selectable, None, 1),
                _ => geometry::next_selectable(&selectable, None, -1),
            };
            if let Some(row) = next { level.ensure_visible(row); }
            level.highlight = None;
            set_highlight(level, next, gfx, colors);
            Step::Continue
        }
        VK_RIGHT => match levels[depth].highlight {
            Some(row) if matches!(levels[depth].items[row], FlyoutItem::Submenu { .. }) => activate(levels, depth, row, gfx, colors, owner, true),
            _ => Step::Continue,
        },
        VK_LEFT | VK_ESCAPE => {
            if depth == 0 { return if vk == VK_ESCAPE { Step::Dismiss } else { Step::Continue }; }
            close_from(levels, depth);
            let parent = &mut levels[depth - 1];
            let row = parent.highlight;
            parent.highlight = None;
            set_highlight(parent, row, gfx, colors);
            Step::Continue
        }
        VK_RETURN | VK_SPACE => match levels[depth].highlight {
            Some(row) => activate(levels, depth, row, gfx, colors, owner, true),
            None => Step::Continue,
        },
        VK_MENU | VK_F10 | VK_LWIN | VK_RWIN | VK_APPS => Step::Dismiss,
        _ => Step::Continue,
    }
}

/// Explicit `&` mnemonics first, then the first letter of each label.
fn mnemonic(levels: &mut Vec<Level>, character: char, gfx: &Gfx, colors: &Palette, owner: HWND) -> Step {
    let depth = levels.len() - 1;
    let wanted = character.to_lowercase().next().unwrap_or(character);
    let level = &levels[depth];
    let explicit: Vec<usize> = (0..level.items.len()).filter(|&i| level.items[i].enabled() && level.texts[i].mnemonic == Some(wanted)).collect();
    let matches = if explicit.is_empty() {
        (0..level.items.len()).filter(|&i| level.items[i].enabled()
            && level.texts[i].label.chars().next().and_then(|c| c.to_lowercase().next()) == Some(wanted)).collect()
    } else { explicit };
    match matches.as_slice() {
        [] => Step::Continue,
        [only] => { let row = *only; set_highlight(&mut levels[depth], Some(row), gfx, colors); activate(levels, depth, row, gfx, colors, owner, true) }
        many => {
            let next = many.iter().copied().find(|&i| Some(i) > levels[depth].highlight).unwrap_or(many[0]);
            levels[depth].ensure_visible(next);
            set_highlight(&mut levels[depth], Some(next), gfx, colors);
            Step::Continue
        }
    }
}

/// Shows `items` and returns the chosen command id. Modal like
/// `TrackPopupMenu`: the owner keeps painting, input goes to the menu, and
/// clicking elsewhere, Alt, Esc or losing the foreground dismisses it.
pub fn track(owner: HWND, items: &[FlyoutItem], anchor: Anchor, dark: bool) -> Option<usize> {
    if items.is_empty() || is_open() { return None; }
    run(owner, items, anchor, dark, &[]).map(|(id, _)| id)
}

/// Like [`track`], but a choice for which `apply` returns new items (a
/// setting that was toggled) keeps the menu open: it is shown again at the
/// same place with the same submenu open and the chosen row highlighted.
pub fn track_persistent(owner: HWND, items: &[FlyoutItem], anchor: Anchor, dark: bool,
        apply: &mut dyn FnMut(usize) -> Option<Vec<FlyoutItem>>) -> Option<usize> {
    if items.is_empty() || is_open() { return None; }
    let mut current = items.to_vec();
    let mut path = Vec::new();
    loop {
        let (id, chosen) = run(owner, &current, anchor, dark, &path)?;
        match apply(id) {
            Some(next) if !next.is_empty() => { current = next; path = chosen; }
            _ => return Some(id),
        }
    }
}

/// Rows of the open submenus, then the highlighted row of the deepest menu.
fn open_path(levels: &[Level]) -> Vec<usize> {
    let mut path: Vec<usize> = levels.iter().filter_map(|level| level.open_child).collect();
    if let Some(row) = levels.last().and_then(|level| level.highlight) { path.push(row); }
    path
}

fn run(owner: HWND, items: &[FlyoutItem], anchor: Anchor, dark: bool, reopen: &[usize]) -> Option<(usize, Vec<usize>)> {
    let gfx = graphics()?;
    let profile = profile();
    let colors = if dark { &profile.dark } else { &profile.light };
    let mut levels = vec![open_level(&gfx, owner, items, Place::Root(anchor))?];
    ACTIVE.with(|active| active.set(true));
    CANCEL.with(|cancel| cancel.set(false));
    struct Reset;
    impl Drop for Reset { fn drop(&mut self) { ACTIVE.with(|active| active.set(false)); CANCEL.with(|cancel| cancel.set(false)); } }
    let _reset = Reset;

    present(&mut levels[0], &gfx, colors);
    // Restore the submenus that were open when a setting was toggled.
    if let Some((&last, parents)) = reopen.split_last() {
        for &row in parents {
            let depth = levels.len() - 1;
            if row >= levels[depth].items.len() || !open_child(&mut levels, depth, row, &gfx, colors, owner) { break; }
        }
        let depth = levels.len() - 1;
        if last < levels[depth].items.len() { set_highlight(&mut levels[depth], Some(last), &gfx, colors); }
    }
    let root = levels[0].hwnd;
    unsafe {
        SendMessageW(owner, WM_ENTERMENULOOP, Some(WPARAM(1)), Some(LPARAM(0)));
        SetCapture(root);
        SetTimer(Some(root), TIMER, 40, None);
    }
    let foreground = unsafe { GetForegroundWindow() };
    let mut start = POINT::default();
    let _ = unsafe { GetCursorPos(&mut start) };
    let drag = (unsafe { GetSystemMetrics(SM_CXDRAG) }.max(2), unsafe { GetSystemMetrics(SM_CYDRAG) }.max(2));
    // The release of the click that opened the menu must not choose an item.
    let mut armed = false;
    let mut pending: Option<(usize, usize, Instant)> = None;
    let mut chosen = None;
    loop {
        if CANCEL.with(Cell::get) { break; }
        let mut msg = MSG::default();
        let status = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if status == 0 { unsafe { PostQuitMessage(msg.wParam.0 as i32) }; break; }
        if status == -1 { break; }
        let point = msg.pt;
        let step = match msg.message {
            WM_MOUSEMOVE => {
                if !armed && ((point.x - start.x).abs() > drag.0 || (point.y - start.y).abs() > drag.1) { armed = true; }
                if let Some((depth, row)) = hit(&levels, point) {
                    if pending.is_some_and(|(at, _, _)| at < depth) { pending = None; }
                    let before = levels[depth].highlight;
                    set_highlight(&mut levels[depth], row, &gfx, colors);
                    let now = levels[depth].highlight;
                    if now != before {
                        // Opening or switching submenus waits for the pointer
                        // to rest, so crossing items toward a submenu is safe.
                        pending = now.map(|row| (depth, row, Instant::now()));
                    }
                } else {
                    // Outside every menu: only the open submenu path stays lit.
                    let last = levels.len() - 1;
                    if levels[last].open_child.is_none() { set_highlight(&mut levels[last], None, &gfx, colors); }
                    pending = None;
                }
                Step::Continue
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK | WM_MBUTTONDBLCLK | WM_XBUTTONDBLCLK => {
                armed = true;
                match hit(&levels, point) {
                    None => Step::Dismiss,
                    Some((depth, Some(row))) if matches!(msg.message, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK) => {
                        set_highlight(&mut levels[depth], Some(row), &gfx, colors);
                        if levels[depth].highlight == Some(row) {
                            levels[depth].pressed = true;
                            present(&mut levels[depth], &gfx, colors);
                            if matches!(levels[depth].items[row], FlyoutItem::Submenu { .. }) {
                                pending = None;
                                activate(&mut levels, depth, row, &gfx, colors, owner, false)
                            } else { Step::Continue }
                        } else { Step::Continue }
                    }
                    Some(_) => Step::Continue,
                }
            }
            WM_LBUTTONUP | WM_RBUTTONUP => match hit(&levels, point) {
                Some((depth, Some(row))) if armed && matches!(levels[depth].items[row], FlyoutItem::Command { .. }) =>
                    activate(&mut levels, depth, row, &gfx, colors, owner, false),
                Some((depth, _)) => { if levels[depth].pressed { levels[depth].pressed = false; present(&mut levels[depth], &gfx, colors); } Step::Continue }
                None => Step::Continue,
            },
            WM_MOUSEWHEEL => {
                let delta = ((msg.wParam.0 >> 16) as u16 as i16) as f32 / 120.0;
                let depth = hit(&levels, point).map_or(levels.len() - 1, |(depth, _)| depth);
                let level = &mut levels[depth];
                let step = level.metrics.px(geometry::ITEM_HEIGHT_DIP) * 3.0;
                let scroll = (level.scroll - delta * step).clamp(0.0, level.max_scroll());
                if scroll != level.scroll {
                    level.scroll = scroll;
                    close_from(&mut levels, depth + 1);
                    let level = &mut levels[depth];
                    let row = level.row_at(point);
                    level.highlight = None;
                    set_highlight(level, row, &gfx, colors);
                    present(level, &gfx, colors);
                }
                Step::Continue
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                pending = None;
                let vk = VIRTUAL_KEY(msg.wParam.0 as u16);
                if NAVIGATION_KEYS.contains(&vk) { key(&mut levels, vk, &gfx, colors, owner) }
                else {
                    // Only character keys are translated (for mnemonics), so
                    // Enter/Esc/Space never leave a WM_CHAR for the owner.
                    unsafe { let _ = TranslateMessage(&msg); }
                    Step::Continue
                }
            }
            WM_CHAR | WM_SYSCHAR => match char::from_u32(msg.wParam.0 as u32).filter(|c| !c.is_control()) {
                Some(character) => mnemonic(&mut levels, character, &gfx, colors, owner),
                None => Step::Continue,
            },
            WM_KEYUP | WM_SYSKEYUP | WM_DEADCHAR | WM_SYSDEADCHAR => Step::Continue,
            WM_TIMER if msg.hwnd == root && msg.wParam.0 == TIMER => {
                if !unsafe { IsWindow(Some(owner)) }.as_bool() || unsafe { GetForegroundWindow() } != foreground {
                    Step::Dismiss
                } else {
                    if let Some((depth, row, since)) = pending {
                        if since.elapsed() >= HOVER_DELAY {
                            pending = None;
                            if depth < levels.len() && levels[depth].highlight == Some(row) && levels[depth].open_child != Some(row) {
                                close_from(&mut levels, depth + 1);
                                if matches!(levels[depth].items[row], FlyoutItem::Submenu { .. }) {
                                    let _ = activate(&mut levels, depth, row, &gfx, colors, owner, false);
                                } else { present(&mut levels[depth], &gfx, colors); }
                            }
                        }
                    }
                    Step::Continue
                }
            }
            _ => {
                unsafe { let _ = TranslateMessage(&msg); DispatchMessageW(&msg); }
                Step::Continue
            }
        };
        match step {
            Step::Continue => {}
            Step::Choose(id) => { chosen = Some(id); break; }
            Step::Dismiss => break,
        }
    }
    unsafe {
        let _ = KillTimer(Some(root), TIMER);
        if GetCapture() == root { let _ = ReleaseCapture(); }
    }
    let path = open_path(&levels);
    close_from(&mut levels, 0);
    // Characters typed into the menu belong to it, not to the owner's focus.
    let mut stale = MSG::default();
    while unsafe { PeekMessageW(&mut stale, None, WM_CHAR, WM_DEADCHAR, PM_REMOVE) }.as_bool() {}
    while unsafe { PeekMessageW(&mut stale, None, WM_SYSCHAR, WM_SYSDEADCHAR, PM_REMOVE) }.as_bool() {}
    unsafe { SendMessageW(owner, WM_EXITMENULOOP, Some(WPARAM(1)), Some(LPARAM(0))); }
    chosen.map(|id| (id, path))
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    #[test]
    #[ignore="hidden menu allocation and offscreen painting only; no presentation or input"]
    fn native_repeated_flyout_painting_releases_menu_windows_and_surfaces() {
        use windows::Win32::System::Ole::{OleInitialize,OleUninitialize};
        unsafe { OleInitialize(None).expect("test COM apartment"); }
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){release_thread_resources();unsafe{OleUninitialize();}}}
        let _apartment=Apartment;
        #[link(name="kernel32")]
        unsafe extern "system" {fn GetCurrentProcess()->*mut core::ffi::c_void;}
        #[link(name="user32")]
        unsafe extern "system" {fn GetGuiResources(process:*mut core::ffi::c_void,flags:u32)->u32;}
        let counts=||unsafe{let process=GetCurrentProcess();[GetGuiResources(process,0),GetGuiResources(process,1)]};
        struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
        let owner=Owner(unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),windows::core::w!("Menu resource fixture"),
            WS_POPUP,-30000,-30000,10,10,None,None,None,None).expect("hidden owner")});
        prepare();let gfx=graphics().expect("graphics");let colors=palette(false);
        let items=vec![FlyoutItem::Command{id:0,label:"Open\tEnter".into(),enabled:true,checked:false,glyph:Some(Glyph{code:"\u{E8B7}",accent:true})},
            FlyoutItem::Separator,FlyoutItem::Command{id:1,label:"Selected".into(),enabled:true,checked:true,glyph:None},
            FlyoutItem::Submenu{label:"More".into(),enabled:true,glyph:None,items:vec![FlyoutItem::Command{id:2,label:"Properties".into(),enabled:true,checked:false,glyph:None}]}];
        let round=||{
            let mut level=open_level(&gfx,owner.0,&items,Place::Root(Anchor::Point{x:0,y:0})).expect("hidden menu");
            assert!(!unsafe{IsWindowVisible(level.hwnd)}.as_bool());
            let shadow=level.metrics.px(geometry::SHADOW_DIP)as i32;
            let surface=Surface::new(level.body.width()+shadow*2,level.body.height()+shadow*2).expect("menu DIB");
            gfx.draw(&surface,|canvas|paint(canvas,&level,&colors)).expect("offscreen menu paint");
            level.surface=Some(surface);
            let hwnd=level.hwnd;let mut levels=vec![level];close_from(&mut levels,0);
            assert!(!unsafe{IsWindow(Some(hwnd))}.as_bool(),"menu HWND remained live");
            let _=unsafe{windows::Win32::Graphics::Gdi::GdiFlush()};
        };
        for _ in 0..8{round();}
        let baseline=counts();
        for _ in 0..256{round();}
        let after=counts();
        assert!(!unsafe{IsWindowVisible(owner.0)}.as_bool());
        assert!(after[0]<=baseline[0]+2&&after[1]<=baseline[1]+2,"repeated flyout resources grew: {baseline:?} -> {after:?}");
        eprintln!("hidden flyout paint cycles=256 GDI/USER {baseline:?} -> {after:?}");
    }

    #[test]
    #[ignore="offscreen Direct2D preparation; creates no menu windows and sends no input"]
    fn native_flyout_thread_cache_releases_after_window_lifetime() {
        use windows::Win32::System::Ole::{OleInitialize,OleUninitialize};
        unsafe { OleInitialize(None).expect("test COM apartment"); }
        struct Apartment;
        impl Drop for Apartment {fn drop(&mut self){unsafe{OleUninitialize();}}}
        let _apartment=Apartment;
        #[link(name="kernel32")]
        unsafe extern "system" { fn GetCurrentProcess() -> *mut core::ffi::c_void; }
        #[link(name="user32")]
        unsafe extern "system" { fn GetGuiResources(process:*mut core::ffi::c_void,flags:u32)->u32; }
        let counts=||unsafe{let process=GetCurrentProcess();[GetGuiResources(process,0),GetGuiResources(process,1)]};
        prepare();release_thread_resources();
        let baseline=counts();
        for _ in 0..64 {
            prepare();
            let graphics=GRAPHICS.with(|slot|Rc::downgrade(slot.borrow().as_ref().expect("Direct2D initialized")));
            let profile=PROFILE.with(|slot|Rc::downgrade(slot.borrow().as_ref().expect("display profile initialized")));
            release_thread_resources();
            assert!(graphics.upgrade().is_none(),"idle STA retained menu rendering resources");
            assert!(profile.upgrade().is_none(),"idle STA retained menu display profile");
        }
        let after=counts();
        assert!(after[0]<=baseline[0]+2 && after[1]<=baseline[1]+2,"menu prepare/release accumulated GDI/USER handles: {baseline:?} -> {after:?}");
        eprintln!("flyout resource cycles=64 GDI/USER {baseline:?} -> {after:?}");
    }
}
