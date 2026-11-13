//! One desktop fence: a translucent, per-pixel-alpha layered window that sits
//! just above the desktop. Geometry and hit-testing are pure; painting uses
//! the shared Direct2D target and is presented with `UpdateLayeredWindow`.

use std::collections::HashSet;

use windows::Win32::{
    Foundation::{HWND, POINT, SIZE, RECT},
    Graphics::{Direct2D::ID2D1Bitmap, Gdi::{CreateRoundRectRgn, GetDC, ReleaseDC, SetWindowRgn, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION}},
    UI::WindowsAndMessaging::{GetWindowRect, UpdateLayeredWindow, ULW_ALPHA},
};
use zsdesk_core::model::desktop::{Fence, FenceView, TITLE_H};
use zsdesk_ui::{chrome::{scale, Rect}, theme::{glyph, FencePalette}};
use zsdesk_windows::{desktop::ShellEntry, gfx::{Align, Canvas, Color, Gfx, RectF, Surface, TextStyle}, shell::notify::ChangeNotify};

pub const EDGE: i32 = 6;

thread_local! {
    /// Frosted glass is on: fence windows are clipped to their rounded shape so
    /// the blur behind them does not show in the transparent corners.
    static BLUR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn set_blur(on: bool) { BLUR.with(|b| b.set(on)); }

/// Applies or removes the frosted-glass blur and the matching rounded window region.
pub fn apply_glass(hwnd: HWND) {
    let on = BLUR.with(std::cell::Cell::get) && !is_desktop_item(hwnd);
    zsdesk_windows::theme::set_blur_behind(hwnd, on);
    shape_window(hwnd);
}

/// Keeps the window region in step with the painted 10-DIP corners (after every resize).
pub fn shape_window(hwnd: HWND) {
    if hwnd.0.is_null() { return; }
    if !BLUR.with(std::cell::Cell::get) || is_desktop_item(hwnd) {
        let _ = unsafe { SetWindowRgn(hwnd, None, true) };
        return;
    }
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err(){return;}
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) }.max(96);
    let diameter = scale(20, dpi);
    // The system owns the region after SetWindowRgn succeeds.
    let region = unsafe { CreateRoundRectRgn(0, 0, r.right - r.left + 1, r.bottom - r.top + 1, diameter, diameter) };
    if region.0.is_null(){return;}
    if unsafe { SetWindowRgn(hwnd, Some(region), true) }==0{let _=unsafe{windows::Win32::Graphics::Gdi::DeleteObject(region.into())};}
}

fn is_desktop_item(hwnd: HWND) -> bool {
    !unsafe { windows::Win32::UI::WindowsAndMessaging::GetPropW(hwnd, windows::core::w!("ZSDesk.DesktopItem")) }.0.is_null()
}

/// What the pointer is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceHit { None, Title, More, Back, Edge(Edges), Item(usize), Empty, Scrollbar, Tab(usize), AddTab, TabList }

/// One tab of the fence's group as measured at the last render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabMeta { pub id: u64, pub label: String, pub width: i32, pub locked: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionChange { All, Clear, Invert }

fn changed_selection<'a>(keys: impl IntoIterator<Item=&'a str>, previous: &HashSet<String>, change: SelectionChange) -> HashSet<String> {
    keys.into_iter().filter(|key| match change {
        SelectionChange::All => true, SelectionChange::Clear => false, SelectionChange::Invert => !previous.contains(*key),
    }).map(str::to_owned).collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges { pub left: bool, pub top: bool, pub right: bool, pub bottom: bool }

/// Pointer interaction in progress.
#[derive(Clone, Debug)]
pub enum Interaction {
    Move { cursor: POINT, rect: RECT },
    Resize { edges: Edges, cursor: POINT, rect: RECT },
    Press { index: usize, cursor: POINT },
    /// Rubber-band selection; `start` is in content coordinates (client y +
    /// scroll at the press), `current` in client coordinates.
    Rubber { start: POINT, current: POINT, base: HashSet<String> },
    Thumb { cursor_y: i32, scroll: i32 },
    /// Pressed on a tab: a click switches to it, a drag reorders or tears it off.
    TabPress { tab: u64, cursor: POINT },
    TabDrag { tab: u64, torn: bool },
}

#[derive(Clone, Copy, Debug)]
pub struct Cell { pub cell: Rect, pub icon: Rect, pub label: Rect }

#[derive(Clone, Debug, Default)]
pub struct Geometry {
    pub width: i32,
    pub height: i32,
    pub title: Rect,
    pub more: Rect,
    pub back: Rect,
    pub content: Rect,
    pub cells: Vec<Cell>,
    pub content_height: i32,
    pub max_scroll: i32,
    pub thumb: Rect,
    pub icon_px: i32,
    /// Visible tab chips: (index into the fence's tabs, rectangle).
    pub tabs: Vec<(usize, Rect)>,
    pub add: Rect,
    /// The "+" keeps its place in the layout but is only drawn and clickable
    /// while the pointer is on the title bar.
    pub add_hidden: bool,
    pub tab_list: Rect,
    pub hidden_tabs: usize,
}

impl Geometry {
    pub fn desktop_item(width: i32, height: i32, dpi: u32, size: i32, count: usize) -> Self {
        let mut geometry = Self { width, height, content: Rect::new(0, 0, width, height), icon_px: scale(size, dpi), ..Default::default() };
        if count != 0 {
            let icon = geometry.icon_px.min(width.saturating_sub(scale(8, dpi))).max(1);
            geometry.cells.push(Cell {
                cell: Rect::new(0, 0, width, height),
                icon: Rect::new((width - icon) / 2, scale(4, dpi), icon, icon),
                label: Rect::new(scale(2, dpi), scale(8, dpi) + icon, (width - scale(4, dpi)).max(0), (height - icon - scale(10, dpi)).max(0)),
            });
        }
        geometry
    }
    pub fn new(width: i32, height: i32, dpi: u32, fence: &Fence, count: usize, scroll: i32, browsing: bool) -> Self {
        let s = |v: i32| scale(v, dpi);
        let title_h = s(TITLE_H);
        let mut g = Geometry { width, height, ..Default::default() };
        g.title = Rect::new(0, 0, width, title_h.min(height));
        g.more = Rect::new(width - s(32), s(3), s(28), title_h - s(6));
        g.back = if browsing { Rect::new(s(4), s(3), s(28), title_h - s(6)) } else { Rect::default() };
        g.content = Rect::new(s(4), title_h, (width - s(8)).max(0), (height - title_h - s(4)).max(0));
        g.icon_px = s(fence.icon_size.pixels());
        if fence.collapsed || g.content.height <= 0 { g.content.height = 0; return g; }
        let pad = s(6);
        let inner_w = (g.content.width - pad * 2).max(1);
        match fence.view {
            FenceView::Icons => {
                let icon = g.icon_px;
                let min_cell = icon + s(36);
                let columns = (inner_w / min_cell).max(1);
                let cell_w = inner_w / columns;
                let label_h = s(34);
                let cell_h = icon + s(10) + label_h;
                for i in 0..count {
                    let (c, r) = ((i as i32) % columns, (i as i32) / columns);
                    let x = g.content.x + pad + c * cell_w;
                    let y = g.content.y + pad + r * cell_h - scroll;
                    let cell = Rect::new(x + s(2), y, cell_w - s(4), cell_h - s(2));
                    let icon_rect = Rect::new(x + (cell_w - icon) / 2, y + s(4), icon, icon);
                    let label = Rect::new(x + s(4), icon_rect.bottom() + s(3), cell_w - s(8), label_h - s(2));
                    g.cells.push(Cell { cell, icon: icon_rect, label });
                }
                let rows = (count as i32 + columns - 1) / columns;
                g.content_height = rows * cell_h + pad * 2;
            }
            FenceView::List => {
                let row_h = s(26);
                let icon = s(20);
                for i in 0..count {
                    let y = g.content.y + pad + i as i32 * row_h - scroll;
                    let cell = Rect::new(g.content.x + pad, y, inner_w, row_h - s(2));
                    let icon_rect = Rect::new(cell.x + s(4), y + (row_h - s(2) - icon) / 2, icon, icon);
                    let label = Rect::new(icon_rect.right() + s(8), y, (cell.right() - icon_rect.right() - s(12)).max(0), row_h - s(2));
                    g.cells.push(Cell { cell, icon: icon_rect, label });
                }
                g.content_height = count as i32 * row_h + pad * 2;
            }
        }
        g.max_scroll = (g.content_height - g.content.height).max(0);
        if g.max_scroll > 0 {
            let track = g.content.height - s(8);
            let thumb_h = ((track as i64 * g.content.height as i64 / g.content_height.max(1) as i64) as i32).max(s(24)).min(track);
            let y = g.content.y + s(4) + ((track - thumb_h) as i64 * scroll as i64 / g.max_scroll.max(1) as i64) as i32;
            g.thumb = Rect::new(width - s(9), y, s(4), thumb_h);
        }
        g
    }

    /// Lays out the pill tab strip. A single tab keeps the centred title and
    /// only offers "+" next to the menu button; tabs that do not fit collapse
    /// into a list button while the active tab stays visible.
    pub fn layout_tabs(&mut self, tabs: &[TabMeta], active: usize, dpi: u32) {
        let s = |v: i32| scale(v, dpi);
        let chip_h = s(28).min(self.title.height);
        let y = (self.title.height - chip_h) / 2;
        let button = s(28);
        if self.more.right() <= 0 { return; }
        if tabs.len() < 2 {
            self.add = Rect::new((self.more.x - s(2) - button).max(0), y, button, chip_h);
            return;
        }
        let gap = s(2);
        let left = if self.back.is_empty() { s(6) } else { self.back.right() + s(4) };
        let right = self.more.x - s(4);
        let total: i32 = tabs.iter().map(|t| t.width + gap).sum();
        let mut visible: Vec<usize> = (0..tabs.len()).collect();
        if left + total + button > right {
            let list_w = s(40);
            let room = right - left - button - list_w - gap * 2;
            visible.clear();
            let mut used = 0;
            for (i, tab) in tabs.iter().enumerate() {
                if used + tab.width + gap > room { break; }
                visible.push(i);
                used += tab.width + gap;
            }
            let active = active.min(tabs.len() - 1);
            if !visible.contains(&active) {
                match visible.last_mut() { Some(last) => *last = active, None => visible.push(active) }
                visible.sort_unstable();
            }
            self.hidden_tabs = tabs.len() - visible.len();
        }
        let reserve = button + if self.hidden_tabs > 0 { s(40) + gap } else { 0 };
        let mut x = left;
        for i in visible {
            let width = tabs[i].width.min((right - x - reserve).max(s(40)));
            self.tabs.push((i, Rect::new(x, y, width, chip_h)));
            x += width + gap;
        }
        if self.hidden_tabs > 0 { self.tab_list = Rect::new(x, y, s(40), chip_h); x += s(40) + gap; }
        self.add = Rect::new(x.min(right - button).max(left), y, button, chip_h);
    }

    pub fn hit(&self, x: i32, y: i32, dpi: u32, locked: bool, collapsed: bool) -> FenceHit {
        let edge = scale(EDGE, dpi);
        if !locked {
            let edges = Edges {
                left: x < edge, right: x >= self.width - edge,
                top: y < edge / 2, bottom: !collapsed && y >= self.height - edge,
            };
            if edges.left || edges.right || edges.top || edges.bottom {
                if !(collapsed && (edges.top || edges.bottom) && !(edges.left || edges.right)) { return FenceHit::Edge(edges); }
            }
        }
        if self.title.contains(x, y) {
            if self.more.contains(x, y) { return FenceHit::More; }
            if !self.back.is_empty() && self.back.contains(x, y) { return FenceHit::Back; }
            if let Some((i, _)) = self.tabs.iter().find(|(_, r)| r.contains(x, y)) { return FenceHit::Tab(*i); }
            if !self.tab_list.is_empty() && self.tab_list.contains(x, y) { return FenceHit::TabList; }
            if !self.add_hidden && !self.add.is_empty() && self.add.contains(x, y) { return FenceHit::AddTab; }
            return FenceHit::Title;
        }
        if !self.thumb.is_empty() && x >= self.thumb.x - scale(4, dpi) && self.content.contains(x, y) { return FenceHit::Scrollbar; }
        if self.content.contains(x, y) {
            if let Some(i) = self.cells.iter().position(|c| c.cell.contains(x, y)) { return FenceHit::Item(i); }
            return FenceHit::Empty;
        }
        FenceHit::None
    }

    /// Item indices intersecting a rubber-band rectangle.
    pub fn in_rect(&self, a: POINT, b: POINT) -> Vec<usize> {
        let (l, r) = (a.x.min(b.x), a.x.max(b.x));
        let (t, bo) = (a.y.min(b.y), a.y.max(b.y));
        self.cells.iter().enumerate().filter(|(_, c)| c.cell.x < r && c.cell.right() > l && c.cell.y < bo && c.cell.bottom() > t).map(|(i, _)| i).collect()
    }

    /// Insertion index for a drop at `(x, y)` (manual ordering).
    pub fn insertion(&self, x: i32, y: i32) -> usize {
        self.cells.iter().position(|c| y < c.cell.y || (y < c.cell.bottom() && x < c.cell.x + c.cell.width / 2)).unwrap_or(self.cells.len())
    }

    /// Columns in icon view (for keyboard navigation).
    pub fn columns(&self) -> usize {
        let first_y = self.cells.first().map(|c| c.cell.y);
        self.cells.iter().take_while(|c| Some(c.cell.y) == first_y).count().max(1)
    }
}

/// Runtime state of one fence window.
pub struct FenceWin {
    pub id: u64,
    pub hwnd: HWND,
    pub dpi: u32,
    pub entries: Vec<ShellEntry>,
    pub selection: HashSet<String>,
    pub anchor: Option<usize>,
    pub focus: Option<usize>,
    pub hover: FenceHit,
    pub scroll: i32,
    pub zoom_remainder: i32,
    /// A loose desktop icon has no fence header, frame, background or tabs.
    pub standalone: bool,
    pub interaction: Option<Interaction>,
    pub surface: Option<Surface>,
    pub notify: Option<ChangeNotify>,
    pub notify_path: Option<std::path::PathBuf>,
    pub drop: Option<zsdesk_windows::ole::DropRegistration>,
    /// Item under a drag (folder target) or `usize::MAX` for the whole fence.
    pub drop_hover: Option<usize>,
    pub rename: Option<(HWND, RenameTarget)>,
    pub message: Option<String>,
    pub tracking: bool,
    /// A collapsed fence temporarily rolled down under the pointer.
    pub peek: bool,
    /// Explicitly collapsed under the pointer: no peek until it leaves.
    pub peek_blocked: bool,
    /// The fence's tab group (empty or one entry for a plain fence).
    pub tabs: Vec<TabMeta>,
    /// Translucent while dragged onto another fence to merge.
    pub ghost: bool,
    /// Title of a fence being dragged onto this one's title bar.
    pub merge_hover: Option<String>,
    /// Tab under a file drag.
    pub drop_tab: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameTarget { Item(String), Title }

impl FenceWin {
    pub fn new(id: u64) -> Self {
        Self { id, hwnd: HWND::default(), dpi: 96, entries: Vec::new(), selection: HashSet::new(), anchor: None, focus: None,
            hover: FenceHit::None, scroll: 0, zoom_remainder: 0, standalone: false, interaction: None, surface: None, notify: None, notify_path: None, drop: None,
            drop_hover: None, rename: None, message: None, tracking: false, peek: false, peek_blocked: false,
            tabs: Vec::new(), ghost: false, merge_hover: None, drop_tab: None }
    }

    /// More than one tab: the title bar shows the tab strip.
    pub fn tabbed(&self) -> bool { self.tabs.len() > 1 }
    pub fn active_index(&self) -> usize { self.tabs.iter().position(|t| t.id == self.id).unwrap_or(0) }

    pub fn window_rect(&self) -> RECT {
        let mut r = RECT::default();
        let _ = unsafe { GetWindowRect(self.hwnd, &mut r) };
        r
    }

    pub fn geometry(&self, fence: &Fence) -> Geometry {
        let r = self.window_rect();
        if self.standalone { return Geometry::desktop_item(r.right-r.left, r.bottom-r.top, self.dpi, fence.icon_size.pixels(), self.entries.len()); }
        let mut g = if self.peek {
            let mut open = fence.clone();
            open.collapsed = false;
            Geometry::new(r.right - r.left, r.bottom - r.top, self.dpi, &open, self.entries.len(), self.scroll, fence.browse.is_some())
        } else {
            Geometry::new(r.right - r.left, r.bottom - r.top, self.dpi, fence, self.entries.len(), self.scroll, fence.browse.is_some())
        };
        g.layout_tabs(&self.tabs, self.active_index(), self.dpi);
        g.add_hidden = !self.title_hovered() || (self.peek && !self.tabbed());
        g
    }

    /// Pointer on the title bar or one of its buttons. A fence rolled down by
    /// hovering shows no "+" until it is a tab group: adding tabs stays in the
    /// fence menu and the strip is not cluttered on every roll-down.
    fn title_hovered(&self) -> bool {
        matches!(self.hover, FenceHit::Title | FenceHit::Tab(_) | FenceHit::AddTab | FenceHit::TabList | FenceHit::More | FenceHit::Back)
    }

    /// Collapsed as displayed right now (a peeking fence behaves as expanded).
    pub fn collapsed(&self, fence: &Fence) -> bool { fence.collapsed && !self.peek }

    pub fn clamp_scroll(&mut self, fence: &Fence) {
        let g = self.geometry(fence);
        self.scroll = self.scroll.clamp(0, g.max_scroll);
    }

    pub fn selected_entries(&self) -> Vec<&ShellEntry> {
        self.entries.iter().filter(|e| self.selection.contains(&e.key)).collect()
    }

    pub fn change_selection(&mut self, change: SelectionChange) {
        self.selection = changed_selection(self.entries.iter().map(|entry| entry.key.as_str()), &self.selection, change);
        if !self.focus.and_then(|index| self.entries.get(index)).is_some_and(|entry| self.selection.contains(&entry.key)) {
            self.focus = self.entries.iter().position(|entry| self.selection.contains(&entry.key));
        }
        self.anchor = self.focus;
    }

    /// Paints and presents the fence. `icon` resolves item bitmaps.
    pub fn render(&mut self, gfx: &Gfx, fence: &Fence, palette: &FencePalette, show_counts: bool, mut icon: impl FnMut(&ShellEntry, i32) -> Option<ID2D1Bitmap>) -> bool {
        if self.hwnd.0.is_null() { return false; }
        let rect = self.window_rect();
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        if w <= 0 || h <= 0 { return false; }
        let g = self.geometry(fence);
        let bitmaps: Vec<Option<ID2D1Bitmap>> = g.cells.iter().enumerate().map(|(i, c)| {
            let visible = c.cell.bottom() >= g.content.y && c.cell.y <= g.content.bottom();
            if visible { icon(&self.entries[i], if fence.view == FenceView::List { scale(20, self.dpi) } else { g.icon_px }) } else { None }
        }).collect();
        if Surface::ensure(&mut self.surface, w, h).is_none() { return false; }
        let Some(surface) = self.surface.as_ref() else { return false };
        let dpi = self.dpi;
        let sf = |v: f32| v * dpi as f32 / 96.0;
        let col = |c: zsdesk_ui::theme::Color| Color::rgba8(c.r, c.g, c.b, c.a);
        let drawn = gfx.draw(surface, |cv: &Canvas| {
            let outer = RectF::new(0.5, 0.5, w as f32 - 1.0, h as f32 - 1.0);
            let radius = sf(10.0);
            let drop_all = self.drop_hover == Some(usize::MAX);
            let hovered = !matches!(self.hover, FenceHit::None);
            if !self.standalone {
            cv.round(outer, radius, col(palette.background));
            // Title bar.
            let title = RectF::new(0.0, 0.0, w as f32, g.title.height as f32);
            cv.push_clip(title);
            cv.round(outer, radius, col(palette.title));
            cv.pop_clip();
            let renaming_title = matches!(self.rename, Some((_, RenameTarget::Title)));
            if self.merge_hover.is_some() {
                cv.push_clip(title);
                cv.round(outer, radius, col(palette.selected).with_alpha(0.6));
                cv.pop_clip();
            }
            if self.tabbed() {
                let dragged = match &self.interaction { Some(Interaction::TabDrag { tab, .. }) => Some(*tab), _ => None };
                for &(i, r) in &g.tabs {
                    let tab = &self.tabs[i];
                    let chip = rf(r);
                    let active = tab.id == self.id;
                    let dropping = self.drop_tab == Some(i);
                    if dropping {
                        cv.round(chip, sf(6.0), col(palette.selected));
                        cv.stroke_round(chip, sf(6.0), col(palette.accent), sf(1.5));
                    } else if active {
                        cv.round(chip, sf(6.0), col(palette.chip));
                    } else if self.hover == FenceHit::Tab(i) {
                        cv.round(chip, sf(6.0), col(palette.hover));
                    }
                    let mut color = if active || dropping { col(palette.text) } else { col(palette.text_secondary) };
                    if dragged == Some(tab.id) { color = color.with_alpha(0.45); }
                    let x = chip.x + sf(10.0);
                    if !(active && renaming_title) {
                        let style = TextStyle::ui(sf(13.0)).weight(if active { 600 } else { 400 });
                        let lock = if tab.locked { sf(16.0) } else { 0.0 };
                        let right = chip.right() - sf(10.0) - lock;
                        cv.text(&tab.label, RectF::new(x, chip.y, (right - x).max(0.0), chip.h), &style, color);
                        if tab.locked { cv.glyph(glyph::LOCK, RectF::new(right + sf(2.0), chip.y, sf(14.0), chip.h), sf(10.0), color); }
                    }
                    if active && !dropping {
                        // Windows 11 selector-bar indicator under the selected tab.
                        let bar = sf(16.0).min(chip.w - sf(12.0)).max(0.0);
                        cv.round(RectF::new(chip.x + (chip.w - bar) / 2.0, chip.bottom() - sf(3.0), bar, sf(3.0)), sf(1.5), col(palette.accent));
                    }
                }
                if !g.tab_list.is_empty() {
                    let list = rf(g.tab_list);
                    if self.hover == FenceHit::TabList { cv.round(list, sf(6.0), col(palette.hover)); }
                    cv.glyph(glyph::CHEVRON_DOWN_SMALL, RectF::new(list.x + sf(6.0), list.y, sf(12.0), list.h), sf(10.0), col(palette.text_secondary));
                    cv.text(&g.hidden_tabs.to_string(), RectF::new(list.x + sf(20.0), list.y, list.w - sf(22.0), list.h),
                        &TextStyle::ui(sf(12.0)), col(palette.text_secondary));
                }
            } else if !renaming_title {
                let title_style = TextStyle::ui(sf(13.0)).weight(600).align(Align::Center);
                let side = if hovered { sf(66.0) } else { sf(36.0) };
                let lock = if fence.locked { sf(16.0) } else { 0.0 };
                let room = (w as f32 - side * 2.0 - lock).max(0.0);
                let area = RectF::new(side + lock / 2.0, 0.0, room, title.h);
                cv.text(&fence.title, area, &title_style, col(palette.text));
                if fence.locked {
                    // Fluent lock glyph after the centred title (not a color emoji).
                    let (text_w, _) = cv.gfx().measure(&fence.title, &title_style, room);
                    let x = area.x + (room + text_w.min(room)) / 2.0 + sf(4.0);
                    cv.glyph(glyph::LOCK, RectF::new(x, 0.0, sf(12.0), title.h), sf(10.0), col(palette.text_secondary));
                }
            }
            if !g.add_hidden && !g.add.is_empty() && self.merge_hover.is_none() {
                let add = rf(g.add);
                if self.hover == FenceHit::AddTab { cv.round(add, sf(6.0), col(palette.hover)); }
                cv.glyph(glyph::ADD, add, sf(11.0), col(palette.text_secondary));
            }
            if let Some(incoming) = &self.merge_hover {
                // Where the dragged fence will land as a tab.
                let width = sf(96.0).min((w as f32 - sf(80.0)).max(sf(40.0)));
                let x = if self.tabbed() { g.add.x as f32 } else { g.more.x as f32 - sf(4.0) - width };
                let chip = RectF::new(x.min(w as f32 - sf(36.0) - width).max(sf(6.0)), (title.h - sf(28.0)) / 2.0, width, sf(28.0));
                cv.stroke_round(chip, sf(6.0), col(palette.accent), sf(1.5));
                cv.text(incoming, chip.inset(sf(10.0), 0.0), &TextStyle::ui(sf(13.0)).weight(600), col(palette.accent));
            }
            let small = TextStyle::ui(sf(11.0)).align(Align::End);
            if hovered && !g.more.is_empty() {
                let more = rf(g.more);
                if self.hover == FenceHit::More { cv.round(more, sf(4.0), col(palette.hover)); }
                cv.glyph(glyph::MORE, more, sf(13.0), col(palette.text));
            } else if show_counts {
                let count = self.entries.len().to_string();
                cv.text(&count, RectF::new(w as f32 - sf(40.0), 0.0, sf(30.0), title.h), &small, col(palette.text_secondary));
            }
            if !g.back.is_empty() {
                let back = rf(g.back);
                if self.hover == FenceHit::Back { cv.round(back, sf(4.0), col(palette.hover)); }
                cv.glyph("\u{E72B}", back, sf(12.0), col(palette.text));
            }
            }
            if g.content.height > 0 {
                let content = rf(g.content);
                cv.push_clip(content);
                if self.entries.is_empty() && !self.standalone {
                    let hint = self.message.clone().unwrap_or_else(|| if fence.is_desktop() { "将桌面图标拖到这里".into() } else { "此文件夹为空".into() });
                    cv.text(&hint, content, &TextStyle::ui(sf(12.0)).align(Align::Center), col(palette.text_secondary));
                }
                let label_style = if fence.view == FenceView::List { TextStyle::ui(sf(12.0)) } else { TextStyle::ui(sf(12.0)).align(Align::Center).top().wrapped() };
                for (i, cell) in g.cells.iter().enumerate() {
                    if cell.cell.bottom() < g.content.y || cell.cell.y > g.content.bottom() { continue; }
                    let entry = &self.entries[i];
                    let selected = self.selection.contains(&entry.key);
                    let r = rf(cell.cell);
                    // Nonzero alpha only over the icon's own cell. The rest of
                    // the desktop remains native and receives clicks normally.
                    if self.standalone { cv.fill(r, Color::rgba8(0, 0, 0, 1)); }
                    if self.drop_hover == Some(i) {
                        cv.round(r, sf(4.0), col(palette.selected));
                        cv.stroke_round(r, sf(4.0), col(palette.accent), sf(1.5));
                    } else if selected {
                        cv.round(r, sf(4.0), col(palette.selected));
                        cv.stroke_round(r, sf(4.0), col(palette.selected_border), 1.0);
                    } else if self.hover == FenceHit::Item(i) {
                        cv.round(r, sf(4.0), col(palette.hover));
                    }
                    if self.focus == Some(i) && selected { cv.stroke_round(r, sf(4.0), col(palette.selected_border), 1.0); }
                    let icon_rect = rf(cell.icon);
                    let opacity = if entry.ghosted { 0.5 } else { 1.0 };
                    match &bitmaps[i] {
                        Some(bitmap) => {
                            let (bw, bh) = bitmap_size(bitmap);
                            cv.image(bitmap, fit(icon_rect, bw, bh), opacity);
                        }
                        None => cv.glyph(if entry.is_container() { glyph::FOLDER } else { "\u{E8A5}" }, icon_rect, icon_rect.h * 0.7, col(palette.text_secondary)),
                    }
                    if entry.is_link && fence.view == FenceView::Icons {
                        // Shortcut overlay arrow, like the desktop.
                        let a = RectF::new(icon_rect.x, icon_rect.bottom() - sf(14.0), sf(14.0), sf(14.0));
                        cv.round(a, sf(2.0), Color::rgba8(0xFF, 0xFF, 0xFF, 0xF0));
                        cv.glyph("\u{E8A7}", a, sf(9.0), Color::rgba8(0x00, 0x5F, 0xB8, 0xFF));
                    }
                    if let Some((_, RenameTarget::Item(key))) = &self.rename { if *key == entry.key { continue; } }
                    let label = rf(cell.label);
                    let shadow = col(palette.text_shadow);
                    if shadow.a > 0.0 && !selected {
                        cv.text(&entry.name, RectF::new(label.x + 1.0, label.y + 1.0, label.w, label.h), &label_style, shadow);
                    }
                    cv.text(&entry.name, label, &label_style, col(palette.text));
                }
                if let Some(Interaction::Rubber { start, current, .. }) = &self.interaction {
                    let start = POINT { x: start.x, y: start.y - self.scroll };
                    let band = RectF::new(start.x.min(current.x) as f32, start.y.min(current.y) as f32,
                        (start.x - current.x).abs() as f32, (start.y - current.y).abs() as f32);
                    cv.fill(band, col(palette.selected));
                    cv.stroke_round(band, 0.0, col(palette.selected_border), 1.0);
                }
                cv.pop_clip();
                if !g.thumb.is_empty() && (hovered || matches!(self.interaction, Some(Interaction::Thumb { .. }))) {
                    cv.round(rf(g.thumb), sf(2.0), col(palette.text_secondary).with_alpha(0.6));
                }
            }
            let highlight = drop_all || self.merge_hover.is_some();
            let border = if highlight { col(palette.accent) } else { col(palette.border) };
            if !self.standalone { cv.stroke_round(outer, radius, border, if highlight { sf(2.0) } else { 1.0 }); }
        });
        if drawn.is_err() { return false; }
        let screen = unsafe { GetDC(None) };
        let alpha = if self.ghost { 150 } else { 255 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
        let position = POINT { x: rect.left, y: rect.top };
        let size = SIZE { cx: w, cy: h };
        let origin = POINT::default();
        let shown = unsafe { UpdateLayeredWindow(self.hwnd, Some(screen), Some(&position), Some(&size), Some(surface.dc), Some(&origin),
            Default::default(), Some(&blend), ULW_ALPHA) };
        unsafe { ReleaseDC(None, screen); }
        shown.is_ok()
    }
}

fn rf(r: Rect) -> RectF { RectF::new(r.x as f32, r.y as f32, r.width as f32, r.height as f32) }

fn bitmap_size(bitmap: &ID2D1Bitmap) -> (f32, f32) {
    let size = unsafe { bitmap.GetPixelSize() };
    (size.width as f32, size.height as f32)
}

/// Fits a bitmap into `rect` keeping its aspect ratio (thumbnails are not square).
fn fit(rect: RectF, w: f32, h: f32) -> RectF {
    if w <= 0.0 || h <= 0.0 { return rect; }
    let scale = (rect.w / w).min(rect.h / h).min(4.0);
    let (fw, fh) = (w * scale, h * scale);
    RectF::new(rect.x + (rect.w - fw) / 2.0, rect.y + (rect.h - fh), fw, fh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zsdesk_core::model::desktop::{FenceSource, Placement};

    fn fence() -> Fence { Fence::new(1, "t", FenceSource::Desktop, Placement::default()) }

    #[test]
    fn inverse_selection_uses_the_complete_original_set() {
        let old = HashSet::from(["a".to_string(), "gone".to_string()]);
        let inverse = changed_selection(["a", "b", "c"], &old, SelectionChange::Invert);
        assert_eq!(inverse, HashSet::from(["b".to_string(), "c".to_string()]));
        assert_eq!(changed_selection(["a", "b", "c"], &inverse, SelectionChange::Invert), HashSet::from(["a".to_string()]));
        assert!(changed_selection(["a", "b"], &old, SelectionChange::Clear).is_empty());
        assert_eq!(changed_selection(["a", "b"], &old, SelectionChange::All), HashSet::from(["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn icon_grid_wraps_and_scrolls() {
        let g = Geometry::new(340, 250, 96, &fence(), 30, 0, false);
        assert_eq!(g.cells.len(), 30);
        assert!(g.columns() >= 3);
        assert!(g.max_scroll > 0);
        assert!(g.cells.iter().all(|c| c.cell.x >= g.content.x && c.cell.right() <= g.content.right()));
        let scrolled = Geometry::new(340, 250, 96, &fence(), 30, 40, false);
        assert_eq!(scrolled.cells[0].cell.y, g.cells[0].cell.y - 40);
    }

    #[test]
    fn released_item_geometry_has_no_fence_chrome_at_multiple_dpi() {
        for dpi in [96,144,192] {
            let geometry=Geometry::desktop_item(scale(96,dpi),scale(104,dpi),dpi,48,1);
            assert!(geometry.title.is_empty()&&geometry.more.is_empty()&&geometry.add.is_empty());
            assert!(geometry.tabs.is_empty()&&geometry.thumb.is_empty());
            assert_eq!(geometry.cells.len(),1);
            let cell=geometry.cells[0];
            assert!(cell.icon.bottom()<=cell.label.y&&cell.label.bottom()<=geometry.height);
            assert_eq!(geometry.hit(cell.icon.x+1,cell.icon.y+1,dpi,true,false),FenceHit::Item(0));
        }
        assert!(Geometry::desktop_item(96,104,96,48,0).cells.is_empty());
    }

    #[test]
    fn hit_testing_prefers_edges_then_title_then_items() {
        let g = Geometry::new(300, 200, 96, &fence(), 3, 0, false);
        assert!(matches!(g.hit(1, 100, 96, false, false), FenceHit::Edge(Edges { left: true, .. })));
        assert_eq!(g.hit(1, 100, 96, true, false), FenceHit::None);
        assert_eq!(g.hit(150, 10, 96, false, false), FenceHit::Title);
        let c = g.cells[1].cell;
        assert_eq!(g.hit(c.x + 3, c.y + 3, 96, false, false), FenceHit::Item(1));
        assert_eq!(g.hit(150, 190, 96, true, false), FenceHit::Empty);
    }

    #[test]
    fn collapsed_fence_has_no_content_or_vertical_resize() {
        let mut f = fence();
        f.collapsed = true;
        let g = Geometry::new(300, TITLE_H, 96, &f, 10, 0, false);
        assert!(g.cells.is_empty());
        assert_eq!(g.hit(150, TITLE_H - 1, 96, false, true), FenceHit::Title);
    }

    fn tabs(widths: &[i32]) -> Vec<TabMeta> {
        widths.iter().enumerate().map(|(i, w)| TabMeta { id: i as u64 + 1, label: format!("t{i}"), width: *w, locked: false }).collect()
    }

    #[test]
    fn single_tab_keeps_the_title_and_offers_add_beside_the_menu() {
        let mut g = Geometry::new(300, 200, 96, &fence(), 0, 0, false);
        g.layout_tabs(&tabs(&[60]), 0, 96);
        assert!(g.tabs.is_empty() && g.tab_list.is_empty());
        assert!(g.add.right() <= g.more.x);
        assert_eq!(g.hit(g.add.x + 2, g.add.y + 2, 96, false, false), FenceHit::AddTab);
        assert_eq!(g.hit(100, 20, 96, false, false), FenceHit::Title, "the centred title still moves the fence");
    }

    #[test]
    fn tabs_fit_in_order_with_add_after_them() {
        let mut g = Geometry::new(400, 200, 96, &fence(), 0, 0, false);
        g.layout_tabs(&tabs(&[60, 70, 80]), 1, 96);
        assert_eq!(g.tabs.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(g.tabs.windows(2).all(|p| p[0].1.right() < p[1].1.x));
        assert!(g.tabs.last().unwrap().1.right() < g.add.x && g.add.right() <= g.more.x);
        let (_, second) = g.tabs[1];
        assert_eq!(g.hit(second.x + 3, second.y + 3, 96, false, false), FenceHit::Tab(1));
        assert_eq!(g.hit(g.add.right() + 4, 20, 96, false, false), FenceHit::Title, "space after the tabs moves the fence");
        g.add_hidden = true;
        assert_eq!(g.hit(g.add.x + 2, g.add.y + 2, 96, false, false), FenceHit::Title, "a hidden + is not clickable");
    }

    #[test]
    fn overflowing_tabs_keep_the_active_tab_and_a_list_button() {
        for dpi in [96, 144] {
            let width = scale(260, dpi);
            let mut g = Geometry::new(width, scale(200, dpi), dpi, &fence(), 0, 0, false);
            let metas = tabs(&[scale(70, dpi); 8]);
            g.layout_tabs(&metas, 6, dpi);
            assert!(g.hidden_tabs > 0 && !g.tab_list.is_empty());
            assert_eq!(g.tabs.len() + g.hidden_tabs, 8);
            assert!(g.tabs.iter().any(|(i, _)| *i == 6));
            let mut rects: Vec<Rect> = g.tabs.iter().map(|(_, r)| *r).collect();
            rects.extend([g.tab_list, g.add]);
            for (n, a) in rects.iter().enumerate() {
                assert!(a.x >= 0 && a.right() <= g.more.x, "{a:?} at {dpi} DPI");
                for b in &rects[n + 1..] { assert!(a.right() <= b.x || b.right() <= a.x, "{a:?} overlaps {b:?}"); }
            }
            assert_eq!(g.hit(g.tab_list.x + 2, g.tab_list.y + 2, dpi, false, false), FenceHit::TabList);
        }
        // Even the narrowest fence shows the active tab.
        let mut g = Geometry::new(160, 200, 96, &fence(), 0, 0, false);
        g.layout_tabs(&tabs(&[90, 90, 90]), 2, 96);
        assert_eq!(g.tabs.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn insertion_index_follows_reading_order() {
        let g = Geometry::new(340, 400, 96, &fence(), 6, 0, false);
        let c = g.cells[1].cell;
        assert_eq!(g.insertion(c.x + 2, c.y + 5), 1);
        assert_eq!(g.insertion(c.right() - 2, c.y + 5), 2);
        assert_eq!(g.insertion(5, 399), 6);
    }

    #[test]
    fn thumbnails_keep_aspect_ratio() {
        let r = fit(RectF::new(0.0, 0.0, 48.0, 48.0), 96.0, 48.0);
        assert_eq!((r.w, r.h), (48.0, 24.0));
        assert_eq!(r.bottom(), 48.0);
    }
}
