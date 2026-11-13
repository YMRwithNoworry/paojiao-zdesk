//! Geometry of Windows 11 style menus (WinUI `MenuFlyout` metrics) without
//! any Win32 dependency, so placement and row layout are unit-testable.

/// Transparent margin around a menu body that holds its drop shadow.
pub const SHADOW_DIP: f32 = 14.0;
/// Vertical offset of the shadow below the body.
pub const SHADOW_OFFSET_DIP: f32 = 3.0;
pub const RADIUS_DIP: f32 = 8.0;
pub const PADDING_DIP: f32 = 4.0;
/// Horizontal inset of the rounded hover plate from the body edge.
pub const INSET_DIP: f32 = 4.0;
pub const ITEM_HEIGHT_DIP: f32 = 32.0;
pub const SEPARATOR_HEIGHT_DIP: f32 = 9.0;
pub const TEXT_DIP: f32 = 14.0;
pub const ACCELERATOR_DIP: f32 = 12.0;
pub const ICON_DIP: f32 = 16.0;
/// Item content padding: icon column, text and trailing edge.
pub const CONTENT_LEFT_DIP: f32 = 12.0;
pub const ICON_GAP_DIP: f32 = 12.0;
pub const CONTENT_RIGHT_DIP: f32 = 12.0;
pub const ACCELERATOR_GAP_DIP: f32 = 24.0;
pub const CHEVRON_DIP: f32 = 20.0;
pub const MIN_WIDTH_DIP: f32 = 160.0;
pub const MAX_WIDTH_DIP: f32 = 460.0;
/// Gap between an anchoring button and a menu opened below it.
pub const ANCHOR_GAP_DIP: f32 = 2.0;
/// How far a submenu overlaps its parent menu.
pub const SUBMENU_OVERLAP_DIP: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics { pub scale: f32 }
impl Metrics {
    pub fn new(dpi: u32) -> Self { Self { scale: dpi.max(96) as f32 / 96.0 } }
    /// Device pixels for a DIP length, rounded so edges stay crisp.
    pub fn px(self, dip: f32) -> f32 { (dip * self.scale).round() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds { pub left: i32, pub top: i32, pub right: i32, pub bottom: i32 }
impl Bounds {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self { Self { left, top, right, bottom } }
    pub fn width(self) -> i32 { self.right - self.left }
    pub fn height(self) -> i32 { self.bottom - self.top }
    pub fn contains(self, x: i32, y: i32) -> bool { x >= self.left && x < self.right && y >= self.top && y < self.bottom }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// Context menu at a screen point (cursor or keyboard location).
    Point { x: i32, y: i32 },
    /// Below (or, without room, above) a button's screen rectangle.
    Below(Bounds),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind { Item, Separator }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row { pub kind: RowKind, pub y: f32, pub h: f32 }

/// Measured text widths of one row, in device pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Measured { pub label: f32, pub accelerator: f32 }

#[derive(Clone, Debug, PartialEq)]
pub struct LevelLayout {
    pub width: f32,
    /// Height of all rows plus vertical padding (may exceed the visible body).
    pub content_height: f32,
    pub rows: Vec<Row>,
    /// Some row shows a glyph or a check mark: all labels share one indent.
    pub icon_column: bool,
    pub has_submenu: bool,
}

impl LevelLayout {
    /// Row under a body-relative point, accounting for the scroll offset.
    pub fn row_at(&self, y: f32, scroll: f32, metrics: Metrics) -> Option<usize> {
        let y = y + scroll;
        let inset = metrics.px(PADDING_DIP);
        if y < inset { return None; }
        self.rows.iter().position(|row| y >= row.y && y < row.y + row.h)
    }
    pub fn text_left(&self, metrics: Metrics) -> f32 {
        let content = metrics.px(INSET_DIP) + metrics.px(CONTENT_LEFT_DIP);
        if self.icon_column { content + metrics.px(ICON_DIP) + metrics.px(ICON_GAP_DIP) } else { content }
    }
}

pub fn layout(kinds: &[RowKind], measured: &[Measured], icon_column: bool, has_submenu: bool, metrics: Metrics) -> LevelLayout {
    let mut rows = Vec::with_capacity(kinds.len());
    let mut y = metrics.px(PADDING_DIP);
    for kind in kinds {
        let h = metrics.px(match kind { RowKind::Item => ITEM_HEIGHT_DIP, RowKind::Separator => SEPARATOR_HEIGHT_DIP });
        rows.push(Row { kind: *kind, y, h });
        y += h;
    }
    let content_height = y + metrics.px(PADDING_DIP);
    let mut probe = LevelLayout { width: 0.0, content_height, rows, icon_column, has_submenu };
    let text_left = probe.text_left(metrics);
    let trailing = metrics.px(CONTENT_RIGHT_DIP) + metrics.px(INSET_DIP) + if has_submenu { metrics.px(CHEVRON_DIP) } else { 0.0 };
    let widest = kinds.iter().zip(measured).filter(|(kind, _)| **kind == RowKind::Item).map(|(_, size)| {
        let accelerator = if size.accelerator > 0.0 { metrics.px(ACCELERATOR_GAP_DIP) + size.accelerator } else { 0.0 };
        text_left + size.label + accelerator + trailing
    }).fold(0.0f32, f32::max);
    probe.width = widest.ceil().clamp(metrics.px(MIN_WIDTH_DIP), metrics.px(MAX_WIDTH_DIP));
    probe
}

/// Visible body height: at most the work area.
pub fn body_height(layout: &LevelLayout, work: Bounds) -> i32 {
    (layout.content_height.ceil() as i32).min(work.height().max(1))
}

/// Screen origin of a root menu body that stays inside `work`, flipping to
/// the other side of the anchor like Windows menus do.
pub fn place_root(anchor: Anchor, width: i32, height: i32, work: Bounds, metrics: Metrics) -> (i32, i32) {
    let (x, y) = match anchor {
        Anchor::Point { x, y } => {
            let x = if x + width > work.right { x - width } else { x };
            let y = if y + height > work.bottom { y - height } else { y };
            (x, y)
        }
        Anchor::Below(button) => {
            let gap = metrics.px(ANCHOR_GAP_DIP) as i32;
            let x = if button.left + width > work.right { button.right - width } else { button.left };
            let below = button.bottom + gap;
            let y = if below + height > work.bottom && button.top - gap - height >= work.top { button.top - gap - height } else { below };
            (x, y)
        }
    };
    clamp_origin(x, y, width, height, work)
}

/// Submenu body origin: beside the parent body, first item level with the
/// parent item, flipping to the left side when the right side has no room.
pub fn place_submenu(parent: Bounds, item_top: i32, width: i32, height: i32, work: Bounds, metrics: Metrics) -> (i32, i32) {
    let overlap = metrics.px(SUBMENU_OVERLAP_DIP) as i32;
    let x = if parent.right - overlap + width > work.right { parent.left + overlap - width } else { parent.right - overlap };
    let y = item_top - metrics.px(PADDING_DIP) as i32;
    clamp_origin(x, y, width, height, work)
}

fn clamp_origin(x: i32, y: i32, width: i32, height: i32, work: Bounds) -> (i32, i32) {
    (x.min(work.right - width).max(work.left), y.min(work.bottom - height).max(work.top))
}

/// Splits a menu label into its text, mnemonic and accelerator text:
/// `"全选(&A)\tCtrl+A"` → `("全选(A)", Some('a'), "Ctrl+A")`; `&&` is a literal `&`.
pub fn parse_label(raw: &str) -> (String, Option<char>, String) {
    let (text, accelerator) = raw.split_once('\t').unwrap_or((raw, ""));
    let mut label = String::with_capacity(text.len());
    let mut mnemonic = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' { label.push(c); continue; }
        match chars.next() {
            Some('&') => label.push('&'),
            Some(next) => { if mnemonic.is_none() { mnemonic = next.to_lowercase().next(); } label.push(next); }
            None => {}
        }
    }
    (label, mnemonic, accelerator.trim().to_string())
}

/// Moves a keyboard highlight by `step` over `selectable` rows, wrapping.
pub fn next_selectable(selectable: &[bool], from: Option<usize>, step: isize) -> Option<usize> {
    let count = selectable.len() as isize;
    if count == 0 || !selectable.iter().any(|value| *value) { return None; }
    let mut index = match from { Some(index) => index as isize, None => if step > 0 { -1 } else { count } };
    for _ in 0..count {
        index = (index + step).rem_euclid(count);
        if selectable[index as usize] { return Some(index as usize); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    const WORK: Bounds = Bounds::new(0, 0, 1920, 1040);

    #[test]
    fn rows_follow_windows11_metrics_at_each_scale() {
        for dpi in [96, 144, 192] {
            let m = Metrics::new(dpi);
            let kinds = [RowKind::Item, RowKind::Separator, RowKind::Item];
            let l = layout(&kinds, &[Measured { label: 40.0, accelerator: 0.0 }; 3], false, false, m);
            assert_eq!(l.rows[0].y, m.px(PADDING_DIP));
            assert_eq!(l.rows[1].h, m.px(SEPARATOR_HEIGHT_DIP));
            assert_eq!(l.content_height, m.px(PADDING_DIP) * 2.0 + m.px(ITEM_HEIGHT_DIP) * 2.0 + m.px(SEPARATOR_HEIGHT_DIP));
            assert_eq!(l.width, m.px(MIN_WIDTH_DIP), "short labels keep the minimum width");
            assert_eq!(l.row_at(l.rows[2].y + 1.0, 0.0, m), Some(2));
            assert_eq!(l.row_at(1.0, 0.0, m), None, "top padding is not a row");
        }
    }

    #[test]
    fn width_reserves_icon_accelerator_and_chevron_space() {
        let m = Metrics::new(96);
        let plain = layout(&[RowKind::Item], &[Measured { label: 200.0, accelerator: 0.0 }], false, false, m);
        let rich = layout(&[RowKind::Item], &[Measured { label: 200.0, accelerator: 60.0 }], true, true, m);
        assert_eq!(rich.width - plain.width, ICON_DIP + ICON_GAP_DIP + ACCELERATOR_GAP_DIP + 60.0 + CHEVRON_DIP);
        assert_eq!(layout(&[RowKind::Item], &[Measured { label: 2000.0, accelerator: 0.0 }], false, false, m).width, MAX_WIDTH_DIP);
        assert_eq!(rich.text_left(m) - plain.text_left(m), ICON_DIP + ICON_GAP_DIP);
    }

    #[test]
    fn root_menus_flip_to_stay_on_screen() {
        let m = Metrics::new(96);
        assert_eq!(place_root(Anchor::Point { x: 100, y: 100 }, 200, 300, WORK, m), (100, 100));
        assert_eq!(place_root(Anchor::Point { x: 1900, y: 1000 }, 200, 300, WORK, m), (1700, 700));
        let button = Bounds::new(500, 40, 560, 72);
        assert_eq!(place_root(Anchor::Below(button), 200, 300, WORK, m), (500, 74));
        let low = Bounds::new(1850, 1000, 1910, 1030);
        assert_eq!(place_root(Anchor::Below(low), 200, 300, WORK, m), (1710, 698), "opens above and right-aligned");
        let tall = layout(&vec![RowKind::Item; 60], &vec![Measured::default(); 60], false, false, m);
        assert_eq!(body_height(&tall, WORK), WORK.height(), "long menus scroll inside the work area");
    }

    #[test]
    fn submenus_open_beside_their_item() {
        let m = Metrics::new(96);
        let parent = Bounds::new(100, 100, 300, 400);
        assert_eq!(place_submenu(parent, 136, 180, 200, WORK, m), (296, 132));
        let right = Bounds::new(1700, 100, 1900, 400);
        assert_eq!(place_submenu(right, 136, 180, 200, WORK, m), (1524, 132), "flips to the left edge");
        assert_eq!(place_submenu(parent, 1000, 180, 200, WORK, m).1, 840, "kept inside the work area");
    }

    #[test]
    fn labels_split_mnemonics_and_accelerators() {
        assert_eq!(parse_label("全选\tCtrl+A"), ("全选".into(), None, "Ctrl+A".into()));
        assert_eq!(parse_label("应用命令(&Z)"), ("应用命令(Z)".into(), Some('z'), String::new()));
        assert_eq!(parse_label("A&&B"), ("A&B".into(), None, String::new()));
        assert_eq!(parse_label("&Open && run&"), ("Open & run".into(), Some('o'), String::new()));
    }

    #[test]
    fn keyboard_navigation_skips_separators_and_disabled_rows() {
        let selectable = [false, true, false, true, true];
        assert_eq!(next_selectable(&selectable, None, 1), Some(1));
        assert_eq!(next_selectable(&selectable, None, -1), Some(4));
        assert_eq!(next_selectable(&selectable, Some(1), 1), Some(3));
        assert_eq!(next_selectable(&selectable, Some(4), 1), Some(1), "wraps");
        assert_eq!(next_selectable(&[false, false], None, 1), None);
    }
}
