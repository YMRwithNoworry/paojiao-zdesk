//! Windows 11 File Explorer style chrome geometry in physical pixels.
//!
//! Vertical order (DIP): tab strip in the title bar (40) → optional bookmarks
//! bar (32, directly under the tabs like 360 文件夹) → address row (48) →
//! command bar (44 per row) → tree | Shell view → optional status bar (26). Everything here is pure arithmetic so it can be tested
//! without a window; the Win32 host only paints and forwards input.

use zsdesk_core::model::explorer::{CommandBarItem, CommandBarPreferences};

pub const STRIP_H: i32 = 40;
pub const TAB_TOP: i32 = 7;
pub const NAV_H: i32 = 48;
pub const COMMAND_H: i32 = 44;
pub const BOOKMARK_H: i32 = 32;
pub const STATUS_H: i32 = 26;
pub const SPLITTER_W: i32 = 5;
pub const TAB_MAX_W: i32 = 232;
pub const TAB_MIN_W: i32 = 64;
pub const MIN_TREE_W: i32 = 120;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect { pub x: i32, pub y: i32, pub width: i32, pub height: i32 }

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self { Self { x, y, width, height } }
    pub fn right(&self) -> i32 { self.x + self.width }
    pub fn bottom(&self) -> i32 { self.y + self.height }
    pub fn contains(&self, x: i32, y: i32) -> bool { x >= self.x && y >= self.y && x < self.right() && y < self.bottom() }
    pub fn is_empty(&self) -> bool { self.width <= 0 || self.height <= 0 }
    fn clamped(x: i32, y: i32, w: i32, h: i32) -> Self { Self { x, y, width: w.max(0), height: h.max(0) } }
}

pub fn scale(value: i32, dpi: u32) -> i32 {
    ((i64::from(value) * i64::from(dpi.max(96)) + 48) / 96) as i32
}

/// Command bar entries, left to right.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandId { New, Cut, Copy, Paste, Rename, Share, Delete, Sort, View, More, PreviewPane, DetailsPane }

impl CommandId {
    pub const fn preference(self) -> CommandBarItem {
        match self {
            Self::New => CommandBarItem::New, Self::Cut => CommandBarItem::Cut, Self::Copy => CommandBarItem::Copy,
            Self::Paste => CommandBarItem::Paste, Self::Rename => CommandBarItem::Rename, Self::Share => CommandBarItem::Share,
            Self::Delete => CommandBarItem::Delete, Self::Sort => CommandBarItem::Sort, Self::View => CommandBarItem::View,
            Self::More => CommandBarItem::More, Self::PreviewPane => CommandBarItem::PreviewPane, Self::DetailsPane => CommandBarItem::DetailsPane,
        }
    }
    /// Label shown next to the icon; icon-only buttons return `None`.
    pub const fn label(self) -> Option<&'static str> {
        match self {
            Self::New => Some("新建"), Self::Sort => Some("排序"), Self::View => Some("查看"),
            Self::PreviewPane => Some("预览"), Self::DetailsPane => Some("详细信息"), _ => None,
        }
    }
    pub const fn tooltip(self) -> &'static str {
        match self {
            Self::New => "新建项目", Self::Cut => "剪切 (Ctrl+X)", Self::Copy => "复制 (Ctrl+C)",
            Self::Paste => "粘贴 (Ctrl+V)", Self::Rename => "重命名 (F2)", Self::Share => "共享",
            Self::Delete => "删除 (Del)", Self::Sort => "排序方式", Self::View => "布局和视图选项",
            Self::More => "查看更多", Self::PreviewPane => "预览窗格 (Alt+P)", Self::DetailsPane => "详细信息窗格 (Alt+Shift+P)",
        }
    }
    pub const fn has_dropdown(self) -> bool { matches!(self, Self::New | Self::Sort | Self::View) }
    const fn width(self) -> i32 {
        match self { Self::New | Self::Sort | Self::View => 82, Self::PreviewPane => 72, Self::DetailsPane => 100, _ => 40 }
    }
}

const COMMAND_GROUPS: &[&[CommandId]] = &[
    &[CommandId::New],
    &[CommandId::Cut, CommandId::Copy, CommandId::Paste, CommandId::Rename, CommandId::Share, CommandId::Delete],
    &[CommandId::Sort, CommandId::View],
    &[CommandId::More],
    &[CommandId::PreviewPane],
];

/// Inputs the host measures or owns.
#[derive(Clone, Debug)]
pub struct ChromeInput<'a> {
    pub tab_placement:zsdesk_core::model::explorer::TabPlacement,
    pub width: i32,
    pub height: i32,
    pub dpi: u32,
    /// Width covered by the DWM caption buttons at the top-right.
    pub caption_width: i32,
    /// Actual minimize-button bounds in client pixels, measured by DWM.
    pub caption_button: Option<Rect>,
    pub tab_count: usize,
    pub active_tab: usize,
    pub show_tree: bool,
    pub tree_width: i32,
    pub show_bookmarks: bool,
    /// Search is active; show the close action beside the fixed native edit.
    pub search_panel_open: bool,
    pub command_bar: &'a CommandBarPreferences,
    /// Own status bar; the hosted Shell view normally draws the native one.
    pub status_bar: bool,
    /// Measured chip widths in pixels (icon + text + padding).
    pub bookmark_widths: &'a [i32],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabSlot { pub index: usize, pub rect: Rect, pub close: Rect, pub show_close: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchPanelHit {
    Category(u8), Scope, Sort, Previous, Next, Clear, Close, Submit, IndexSettings, Retry,
}

/// Search action geometry shared by native edit placement and hit testing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchPanelLayout {
    pub card: Rect,
    pub title: Rect,
    pub query: Rect,
    pub submit: Rect,
    pub clear: Rect,
    pub close: Rect,
    pub scope: Rect,
    pub categories: Vec<Rect>,
    pub status: Rect,
    pub sort: Rect,
    pub previous: Rect,
    pub page: Rect,
    pub next: Rect,
    pub index_settings: Rect,
    pub retry: Rect,
}

impl SearchPanelLayout {
    fn new(width: i32, top: i32, dpi: u32) -> Self {
        let s = |v| scale(v, dpi);
        let margin = s(10).min(width / 2);
        let x = margin + s(12);
        let right = (width - margin - s(12)).max(x);
        let inner = right - x;
        let mut y = top + s(8) + s(12);
        let mut panel = Self::default();
        panel.close = Rect::clamped(right - s(28), y, s(28), s(26));
        panel.title = Rect::clamped(x, y, s(98).min(inner), s(26));
        panel.scope = Rect::clamped(panel.title.right() + s(8), y,
            panel.close.x - s(8) - panel.title.right() - s(8), s(26));
        y += s(32);
        panel.clear = Rect::clamped(right - s(30), y + s(4), s(30), s(30));
        panel.query = Rect::clamped(x, y, panel.clear.x - s(8) - x, s(38));
        panel.submit = Rect::clamped(panel.query.right() - s(32), y + s(3), s(30), s(32));
        y += s(48);
        let mut cx = x;
        for dip_width in [52, 68, 56, 56, 56, 56] {
            let chip_width = s(dip_width).min(inner);
            if cx > x && cx + chip_width > right { cx = x; y += s(34); }
            panel.categories.push(Rect::clamped(cx, y, chip_width, s(28)));
            cx += chip_width + s(6);
        }
        y += s(34);
        panel.retry = Rect::clamped(right - s(56), y, s(56), s(22));
        panel.status = Rect::clamped(x, y, panel.retry.x - s(8) - x, s(22));
        y += s(26);
        // The fixed-width controls fit the 440 DIP content region beside side tabs.
        panel.sort = Rect::clamped(x, y, s(112).min(inner), s(28));
        panel.index_settings = Rect::clamped(panel.sort.right() + s(6), y, s(88), s(28));
        panel.next = Rect::clamped(right - s(28), y, s(28), s(28));
        panel.page = Rect::clamped(panel.next.x - s(4) - s(68), y, s(68), s(28));
        panel.previous = Rect::clamped(panel.page.x - s(4) - s(28), y, s(28), s(28));
        if panel.index_settings.right() + s(10) > panel.previous.x {
            y += s(34);
            panel.previous.y = y;
            panel.page.y = y;
            panel.next.y = y;
        }
        panel.card = Rect::clamped(margin, top + s(8), width - margin * 2,
            y + s(28) + s(12) - (top + s(8)));
        panel
    }

    fn translate_x(&mut self, dx: i32) {
        for rect in [&mut self.card, &mut self.title, &mut self.query, &mut self.submit,
            &mut self.clear, &mut self.close, &mut self.scope, &mut self.status, &mut self.sort,
            &mut self.previous, &mut self.page, &mut self.next, &mut self.index_settings, &mut self.retry] {
            rect.x += dx;
        }
        for rect in &mut self.categories { rect.x += dx; }
    }

    pub fn hit(&self, x: i32, y: i32) -> Hit {
        if !self.card.contains(x, y) { return Hit::None; }
        for (rect, action) in [(self.submit, SearchPanelHit::Submit), (self.clear, SearchPanelHit::Clear),
            (self.close, SearchPanelHit::Close), (self.scope, SearchPanelHit::Scope),
            (self.sort, SearchPanelHit::Sort), (self.previous, SearchPanelHit::Previous),
            (self.next, SearchPanelHit::Next), (self.index_settings, SearchPanelHit::IndexSettings),
            (self.retry, SearchPanelHit::Retry)] {
            if rect.contains(x, y) { return Hit::SearchPanel(action); }
        }
        if self.query.contains(x, y) { return Hit::Search; }
        self.categories.iter().position(|r| r.contains(x, y))
            .map_or(Hit::None, |index| Hit::SearchPanel(SearchPanelHit::Category(index as u8)))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChromeLayout {
    pub dpi: u32,
    pub strip: Rect,
    pub tab_panel: Rect,
    pub vertical_tabs: bool,
    pub tabs: Vec<TabSlot>,
    pub new_tab: Rect,
    pub tab_list: Rect,
    pub app_menu: Rect,
    pub nav_row: Rect,
    pub back: Rect,
    pub forward: Rect,
    pub up: Rect,
    pub refresh: Rect,
    pub address: Rect,
    pub search: Rect,
    pub search_panel: SearchPanelLayout,
    pub command_row: Rect,
    pub commands: Vec<(CommandId, Rect)>,
    /// Icon-only command buttons when their normal labels do not fit one row.
    pub compact_commands: bool,
    pub separators: Vec<Rect>,
    pub bookmarks_row: Rect,
    pub bookmarks: Vec<Rect>,
    pub bookmark_overflow: Rect,
    pub tree: Rect,
    pub splitter: Rect,
    pub browser: Rect,
    pub other_browser: Rect,
    pub pane_splitter: Rect,
    pub pane_headers: [Rect; 2],
    pub status: Rect,
    pub status_details: Rect,
    pub status_tiles: Rect,
    pub status_dual_pane: Rect,
    /// Bottom of the top chrome (start of the content area).
    pub top: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    None,
    /// Empty tab strip: dragged as the window caption.
    Caption,
    Tab(usize),
    TabClose(usize),
    NewTab,
    TabList,
    AppMenu,
    Back,
    Forward,
    Up,
    Refresh,
    Address,
    Search,
    SearchPanel(SearchPanelHit),
    Command(CommandId),
    Bookmark(usize),
    BookmarkOverflow,
    BookmarksBar,
    Splitter,
    PaneSplitter,
    PaneHeader(usize),
    StatusDetails,
    StatusTiles,
    StatusDualPane,
    Status,
}

impl ChromeLayout {
    pub fn new(input: &ChromeInput<'_>) -> Self {
        let dpi = input.dpi.max(96);
        let s = |v: i32| scale(v, dpi);
        let width = input.width.max(0);
        let height = input.height.max(0);
        let mut layout = ChromeLayout { dpi, ..Default::default() };

        // Tab strip (title bar).
        let caption = input.caption_button.filter(|r| !r.is_empty());
        let strip_height = s(STRIP_H).max(caption.map_or(0, |r| r.bottom()));
        layout.strip = Rect::clamped(0, 0, width, strip_height);
        let tab_y = s(TAB_TOP);
        let tab_h = strip_height - tab_y;
        let button = caption.map_or(s(46), |r| r.width);
        let button_h = caption.map_or(s(STRIP_H), |r| r.height);
        let button_y = caption.map_or(0, |r| r.y);
        let caption_left = caption.map_or(width - input.caption_width.max(0), |r| r.x).clamp(0, width);
        let right_edge = (caption_left - s(4)).max(0);
        if right_edge >= button {
            layout.app_menu = Rect::clamped(right_edge - button, button_y, button, button_h);
            if layout.app_menu.x >= button + s(2) {
                layout.tab_list = Rect::clamped(layout.app_menu.x - s(2) - button, button_y, button, button_h);
            }
        }
        let drag_space = s(40);
        let tabs_left = s(8);
        let utility_left = if !layout.tab_list.is_empty() { layout.tab_list.x } else { layout.app_menu.x };
        let tabs_right = (utility_left - drag_space - button - s(4)).max(tabs_left);
        let available = tabs_right - tabs_left;
        let count = input.tab_count.max(1) as i32;
        let natural = (available / count).clamp(s(TAB_MIN_W), s(TAB_MAX_W)).min(available);
        let visible = if available > 0 { ((available / natural.max(1)).max(1) as usize).min(input.tab_count.max(1)) } else { 0 };
        let first = first_visible(input.tab_count, input.active_tab, visible);
        let mut x = tabs_left;
        for index in first..(first + visible).min(input.tab_count) {
            let rect = Rect::clamped(x, tab_y, natural, tab_h);
            let close = Rect::clamped(rect.right() - s(8) - s(24), tab_y + (tab_h - s(24)) / 2, s(24), s(24));
            let show_close = natural >= s(96) || index == input.active_tab;
            layout.tabs.push(TabSlot { index, rect, close, show_close });
            x += natural;
        }
        if x + s(4) + button <= utility_left {
            layout.new_tab = Rect::clamped(x + s(4), tab_y, button, tab_h);
        }

        let full_width=width;
        let panel_width=if input.tab_placement==zsdesk_core::model::explorer::TabPlacement::Top{0}else{s(210).min((width-s(440)).max(s(140))).min(width)};
        let content_offset=if input.tab_placement==zsdesk_core::model::explorer::TabPlacement::Left{panel_width}else{0};
        let width=(width-panel_width).max(0);
        // Bookmarks bar, directly under the tabs.
        let mut nav_y = layout.strip.bottom();
        if input.show_bookmarks {
            layout.bookmarks_row = Rect::clamped(0, nav_y, width, s(BOOKMARK_H));
            let chip_h = s(26);
            let chip_y = nav_y + (s(BOOKMARK_H) - chip_h) / 2;
            let overflow_w = s(32);
            let limit = width - s(8);
            let total: i32 = input.bookmark_widths.iter().map(|w| w + s(2)).sum();
            let reserve = if s(8) + total > limit { overflow_w + s(4) } else { 0 };
            let mut bx = s(8);
            for &w in input.bookmark_widths {
                if bx + w > limit - reserve { break; }
                layout.bookmarks.push(Rect::clamped(bx, chip_y, w, chip_h));
                bx += w + s(2);
            }
            if reserve > 0 { layout.bookmark_overflow = Rect::clamped(width - s(8) - overflow_w, chip_y, overflow_w, chip_h); }
            nav_y = layout.bookmarks_row.bottom();
        }

        // Address row.
        layout.nav_row = Rect::clamped(0, nav_y, width, s(NAV_H));
        let nav_button = s(36);
        let nb_y = nav_y + (s(NAV_H) - s(32)) / 2;
        let mut nx = s(8);
        for slot in [&mut layout.back, &mut layout.forward, &mut layout.up, &mut layout.refresh] {
            *slot = Rect::clamped(nx, nb_y, nav_button, s(32));
            nx += nav_button + s(2);
        }
        let search_w = (width / 4).clamp(s(140), s(280));
        let field_h = s(32);
        let field_y = nav_y + (s(NAV_H) - field_h) / 2;
        layout.search = Rect::clamped(width - s(12) - search_w, field_y, search_w, field_h);
        let address_x = nx + s(6);
        let address_right = layout.search.x - s(8);
        layout.address = Rect::clamped(address_x, field_y, address_right - address_x, field_h);

        // Command bar: compact labels first, then wrap without losing actions.
        let cmd_y = layout.nav_row.bottom();
        let groups: Vec<(usize, Vec<CommandId>)> = COMMAND_GROUPS.iter().enumerate().filter_map(|(index, group)| {
            let items: Vec<CommandId> = group.iter().copied().filter(|id| input.command_bar.item_visible(id.preference())).collect();
            (!items.is_empty()).then_some((index, items))
        }).collect();
        let margin = s(12).min(width / 2);
        let limit = (width - margin).max(margin);
        let available = limit - margin;
        let gap = s(2);
        let group_gap = s(13);
        let count: usize = groups.iter().map(|(_, items)| items.len()).sum();
        let natural_width: i32 = groups.iter().flat_map(|(_, items)| items).map(|id| s(id.width())).sum::<i32>()
            + gap * count.saturating_sub(1) as i32 + group_gap * groups.len().saturating_sub(1) as i32;
        let compact = natural_width > available;
        layout.compact_commands = compact;
        let cb_h = s(32);
        let mut row = 0;
        let mut cx = margin;
        if input.command_bar.visible && available > 0 {
            for (group_index, group) in &groups {
                let button_width = |id: CommandId| if compact { s(40) } else { s(id.width()) };
                let first_width = button_width(group[0]).min(available);
                let panes_on_right = *group_index == COMMAND_GROUPS.len() - 1 && !compact;
                if panes_on_right {
                    let pane_width = group.iter().map(|id| button_width(*id)).sum::<i32>() + gap * (group.len() - 1) as i32;
                    cx = (limit - pane_width).max(cx);
                } else if cx > margin {
                    if cx + group_gap + first_width > limit {
                        row += 1;
                        cx = margin;
                    } else {
                        layout.separators.push(Rect::clamped(cx + s(6), cmd_y + row * s(COMMAND_H) + (s(COMMAND_H) - s(20)) / 2,
                            s(1).max(1), s(20)));
                        cx += group_gap;
                    }
                }
                for &id in group {
                    let w = button_width(id).min(available);
                    if cx > margin && cx + w > limit { row += 1; cx = margin; }
                    let cy = cmd_y + row * s(COMMAND_H) + (s(COMMAND_H) - cb_h) / 2;
                    layout.commands.push((id, Rect::clamped(cx, cy, w, cb_h)));
                    cx += w + gap;
                }
            }
        }
        let command_height = if layout.commands.is_empty() { 0 } else { (row + 1) * s(COMMAND_H) };
        layout.command_row = Rect::clamped(0, cmd_y, width, command_height);
        layout.top = layout.command_row.bottom().min(height);
        // The native edit stays beside the address bar while results replace
        // the current folder view. Search never inserts a second input panel.
        layout.search_panel.query = layout.search;
        layout.search_panel.scope = layout.search;
        layout.search_panel.sort = layout.command_rect(CommandId::Sort).unwrap_or(layout.search);
        if input.search_panel_open {
            layout.search_panel.close = Rect::clamped(layout.search.right()-s(34),field_y,s(30),field_h);
        }

        // Status bar and content.
        let status_h = if input.status_bar { s(STATUS_H) } else { 0 };
        let status_y = (height - status_h).max(layout.top);
        layout.status = Rect::clamped(0, status_y, width, height - status_y);
        let toggle = Rect::clamped(0, status_y + (status_h - s(22)) / 2, s(28), s(22));
        layout.status_tiles = Rect { x: width - s(8) - toggle.width, ..toggle };
        layout.status_details = Rect { x: layout.status_tiles.x - s(2) - toggle.width, ..toggle };
        layout.status_dual_pane = Rect { x: layout.status_details.x - s(2) - toggle.width, ..toggle };
        let content_h = (status_y - layout.top).max(0);
        if input.show_tree && width > s(MIN_TREE_W) * 2 {
            let tree_w = input.tree_width.clamp(s(MIN_TREE_W), (width / 2).max(s(MIN_TREE_W)));
            layout.tree = Rect::clamped(0, layout.top, tree_w, content_h);
            layout.splitter = Rect::clamped(tree_w, layout.top, s(SPLITTER_W), content_h);
            let bx = layout.splitter.right();
            layout.browser = Rect::clamped(bx, layout.top, width - bx, content_h);
        } else {
            layout.browser = Rect::clamped(0, layout.top, width, content_h);
        }
        if panel_width>0{
            layout.vertical_tabs=true;
            let x=if input.tab_placement==zsdesk_core::model::explorer::TabPlacement::Left{0}else{full_width-panel_width};
            layout.tab_panel=Rect::clamped(x,layout.strip.bottom(),panel_width,height-layout.strip.bottom());
            layout.tabs.clear();
            let row=s(36);let top=layout.tab_panel.y+s(6);
            let bottom=(layout.tab_panel.bottom()-row-s(10)).max(top);
            let visible=((bottom-top)/row).max(0)as usize;
            let first=first_visible(input.tab_count,input.active_tab,visible);
            for (position,index) in (first..(first+visible).min(input.tab_count)).enumerate(){
                let rect=Rect::clamped(x+s(6),top+position as i32*row,panel_width-s(12),row-s(2));
                let close=Rect::clamped(rect.right()-s(28),rect.y+(rect.height-s(24))/2,s(24),s(24));
                layout.tabs.push(TabSlot{index,rect,close,show_close:true});
            }
            layout.new_tab=Rect::clamped(x+s(6),(layout.tab_panel.bottom()-row-s(4)).max(layout.tab_panel.y),panel_width-s(12),row);
            for rect in [&mut layout.nav_row,&mut layout.back,&mut layout.forward,&mut layout.up,&mut layout.refresh,&mut layout.address,&mut layout.search,
                &mut layout.command_row,&mut layout.bookmarks_row,&mut layout.bookmark_overflow,&mut layout.tree,&mut layout.splitter,&mut layout.browser,
                &mut layout.status,&mut layout.status_details,&mut layout.status_tiles,&mut layout.status_dual_pane]{rect.x+=content_offset;}
            for (_,rect) in &mut layout.commands{rect.x+=content_offset;}
            for rect in layout.bookmarks.iter_mut().chain(layout.separators.iter_mut()){rect.x+=content_offset;}
            layout.search_panel.translate_x(content_offset);
        }
        layout
    }

    /// Classifies a client point.
    pub fn hit(&self, x: i32, y: i32) -> Hit {
        if self.strip.contains(x, y)||self.tab_panel.contains(x,y) {
            for slot in &self.tabs {
                if slot.rect.contains(x, y) {
                    if slot.show_close && slot.close.contains(x, y) { return Hit::TabClose(slot.index); }
                    return Hit::Tab(slot.index);
                }
            }
            if self.new_tab.contains(x, y) { return Hit::NewTab; }
            if self.tab_list.contains(x, y) { return Hit::TabList; }
            if self.app_menu.contains(x, y) { return Hit::AppMenu; }
            return if self.strip.contains(x,y){Hit::Caption}else{Hit::None};
        }
        if self.nav_row.contains(x, y) {
            if self.search_panel.close.contains(x,y) { return Hit::SearchPanel(SearchPanelHit::Close); }
            for (rect, hit) in [(self.back, Hit::Back), (self.forward, Hit::Forward), (self.up, Hit::Up),
                (self.refresh, Hit::Refresh), (self.address, Hit::Address), (self.search, Hit::Search)] {
                if rect.contains(x, y) { return hit; }
            }
            return Hit::None;
        }
        if self.command_row.contains(x, y) {
            return self.commands.iter().find(|(_, r)| r.contains(x, y)).map_or(Hit::None, |(id, _)| Hit::Command(*id));
        }
        if self.search_panel.card.contains(x, y) { return self.search_panel.hit(x, y); }
        if self.bookmarks_row.contains(x, y) {
            if let Some(i) = self.bookmarks.iter().position(|r| r.contains(x, y)) { return Hit::Bookmark(i); }
            if self.bookmark_overflow.contains(x, y) { return Hit::BookmarkOverflow; }
            return Hit::BookmarksBar;
        }
        if self.splitter.contains(x, y) { return Hit::Splitter; }
        if self.pane_splitter.contains(x, y) { return Hit::PaneSplitter; }
        for (index, rect) in self.pane_headers.iter().enumerate() {
            if rect.contains(x, y) { return Hit::PaneHeader(index); }
        }
        if !self.status.is_empty() && self.status.contains(x, y) {
            if self.status_details.contains(x, y) { return Hit::StatusDetails; }
            if self.status_tiles.contains(x, y) { return Hit::StatusTiles; }
            if self.status_dual_pane.contains(x, y) { return Hit::StatusDualPane; }
            return Hit::Status;
        }
        Hit::None
    }

    pub fn command_rect(&self, id: CommandId) -> Option<Rect> {
        self.commands.iter().find(|(c, _)| *c == id).map(|(_, r)| *r)
    }

    /// Keep visible tabs stationary when selecting or reordering a tab that is
    /// already on screen. Scroll only far enough to reveal an off-screen tab.
    pub fn retain_tab_viewport(&mut self, previous: &Self, count: usize, active: usize) {
        if self.vertical_tabs != previous.vertical_tabs || self.tabs.is_empty() { return; }
        if self.vertical_tabs && (self.tab_panel.x == 0) != (previous.tab_panel.x == 0) { return; }
        let Some(first) = previous.tabs.first().map(|tab| tab.index) else { return };
        let visible = self.tabs.len();
        let active = active.min(count.saturating_sub(1));
        let mut first = first.min(count.saturating_sub(visible));
        if active < first { first = active; }
        else if active >= first + visible { first = active + 1 - visible; }
        for (offset, slot) in self.tabs.iter_mut().enumerate() {
            slot.index = first + offset;
            slot.show_close = self.vertical_tabs || slot.rect.width >= scale(96, self.dpi) || slot.index == active;
        }
    }

    /// A drag may move along the tab axis, but must not reorder tabs while the
    /// pointer is in the address bar, folder tree or opposite edge of the window.
    pub fn tab_drag_target(&self, x: i32, y: i32) -> Option<usize> {
        if self.vertical_tabs {
            if x < self.tab_panel.x || x >= self.tab_panel.right() { return None; }
            self.tabs.iter().min_by_key(|tab| (i64::from(y) - i64::from(tab.rect.y + tab.rect.height / 2)).abs()).map(|tab| tab.index)
        } else {
            if y < self.strip.y || y >= self.strip.bottom() { return None; }
            self.tabs.iter().min_by_key(|tab| (i64::from(x) - i64::from(tab.rect.x + tab.rect.width / 2)).abs()).map(|tab| tab.index)
        }
    }

    /// Tab index whose slot center is nearest to `x` (used while dragging tabs).
    pub fn tab_index_at(&self, x: i32) -> Option<usize> {
        let first = self.tabs.first()?;
        if x < first.rect.x { return Some(first.index); }
        self.tabs.iter().find(|t| x < t.rect.right()).or(self.tabs.last()).map(|t| t.index)
    }
}

/// First visible tab so that `active` stays on screen.
fn first_visible(count: usize, active: usize, visible: usize) -> usize {
    if count <= visible { return 0; }
    let active = active.min(count - 1);
    if active < visible { 0 } else { (active + 1 - visible).min(count - visible) }
}

/// Explorer-style size text ("1.20 MB").
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["字节", "KB", "MB", "GB", "TB"];
    if bytes < 1024 { return format!("{bytes} 字节"); }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() { value /= 1024.0; unit += 1; }
    if value >= 100.0 { format!("{value:.0} {}", UNITS[unit]) }
    else if value >= 10.0 { format!("{value:.1} {}", UNITS[unit]) }
    else { format!("{value:.2} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    static DEFAULT_COMMAND_BAR: CommandBarPreferences = CommandBarPreferences { visible: true, hidden_items: Vec::new() };

    fn input(width: i32, tabs: usize, active: usize, dpi: u32) -> ChromeInput<'static> {
        ChromeInput { tab_placement:zsdesk_core::model::explorer::TabPlacement::Top, width, height: 800, dpi, caption_width: scale(138, dpi), caption_button: None,
            tab_count: tabs, active_tab: active, show_tree: true, tree_width: scale(220, dpi), show_bookmarks: true,
            command_bar: &DEFAULT_COMMAND_BAR, status_bar: true, bookmark_widths: &[], search_panel_open: false }
    }

    #[test]
    fn search_preserves_address_field_and_result_geometry() {
        use zsdesk_core::model::explorer::TabPlacement;
        for dpi in [96, 144, 192] { for width in [640, 960, 1280] {
            for placement in [TabPlacement::Top, TabPlacement::Left, TabPlacement::Right] {
                let layout = ChromeLayout::new(&ChromeInput { search_panel_open: true, tab_placement: placement,
                    height: scale(900, dpi), ..input(scale(width, dpi), 6, 2, dpi) });
                let idle=ChromeLayout::new(&ChromeInput { tab_placement:placement,
                    height:scale(900,dpi),..input(scale(width,dpi),6,2,dpi) });
                assert_eq!(layout.search,idle.search);
                assert_eq!(layout.address,idle.address);
                assert_eq!(layout.browser,idle.browser);
                assert_eq!(layout.tree,idle.tree);
                assert!(layout.search_panel.card.is_empty());
                assert!(layout.address.right()<layout.search.x);
                assert_eq!(layout.hit(layout.search.x+1,layout.search.y+1),Hit::Search);
                let close=layout.search_panel.close;
                assert!(!close.is_empty());
                assert_eq!(layout.hit(close.x+1,close.y+1),Hit::SearchPanel(SearchPanelHit::Close));
            }
        }}
    }

    #[test]
    fn expanded_search_wraps_filters_in_a_constrained_content_region() {
        let panel = SearchPanelLayout::new(340, 0, 96);
        assert_eq!(panel.categories.len(), 6);
        assert!(panel.categories.last().unwrap().y > panel.categories[0].y);
        assert!(panel.previous.y > panel.index_settings.y);
        assert!(panel.categories.iter().all(|chip| chip.x >= panel.card.x && chip.right() <= panel.card.right()));
    }

    #[test]
    fn content_fills_below_chrome_at_all_dpis() {
        for dpi in [96, 120, 144, 192] {
            let l = ChromeLayout::new(&input(1400, 3, 0, dpi));
            assert_eq!(l.top, scale(STRIP_H + NAV_H + COMMAND_H + BOOKMARK_H, dpi));
            assert_eq!(l.bookmarks_row.y, l.strip.bottom(), "bookmarks sit directly under the tabs");
            assert_eq!(l.nav_row.y, l.bookmarks_row.bottom());
            // Pane toggles appear when there is room and never overlap.
            if let (Some(p), Some(d)) = (l.command_rect(CommandId::PreviewPane), l.command_rect(CommandId::DetailsPane)) { assert!(p.right() < d.x); }
            if dpi == 96 { assert!(l.command_rect(CommandId::PreviewPane).is_some()); }
            assert_eq!(l.tree.y, l.top);
            assert_eq!(l.browser.right(), 1400);
            assert_eq!(l.browser.bottom(), l.status.y);
            assert_eq!(l.status.bottom(), 800);
            assert!(l.address.right() < l.search.x);
            assert!(l.tabs.iter().all(|t| t.rect.right() <= l.new_tab.x));
            assert!(l.app_menu.right() <= 1400 - scale(138, dpi));
        }
    }

    #[test]
    fn side_tabs_keep_active_tab_and_content_in_separate_regions(){
        use zsdesk_core::model::explorer::TabPlacement;
        for dpi in [96,120,144,192]{for placement in [TabPlacement::Left,TabPlacement::Right]{
            let l=ChromeLayout::new(&ChromeInput{tab_placement:placement,..input(scale(1200,dpi),120,119,dpi)});
            assert!(l.vertical_tabs&&!l.tab_panel.is_empty());
            let active=l.tabs.iter().find(|tab|tab.index==119).unwrap();
            assert_eq!(l.hit(active.rect.x+4,active.rect.y+4),Hit::Tab(119));
            assert_eq!(l.hit(active.close.x+3,active.close.y+3),Hit::TabClose(119));
            assert_eq!(l.hit(l.tab_list.x+3,l.tab_list.y+3),Hit::TabList);
            if placement==TabPlacement::Left{assert!(l.browser.x>=l.tab_panel.right());assert!(l.address.x>=l.tab_panel.right());}
            else{assert!(l.browser.right()<=l.tab_panel.x);assert!(l.search.right()<=l.tab_panel.x);}
            assert!(l.browser.width>0&&l.new_tab.bottom()<=l.tab_panel.bottom());
        }}
    }

    #[test]
    fn selecting_and_dragging_visible_side_tabs_keeps_the_viewport_stationary() {
        use zsdesk_core::model::explorer::TabPlacement;
        for placement in [TabPlacement::Left, TabPlacement::Right] {
            let before = ChromeLayout::new(&ChromeInput { tab_placement: placement, ..input(1100, 100, 99, 96) });
            let first = before.tabs[0].index;
            let active = first + 1;
            let mut after = ChromeLayout::new(&ChromeInput { tab_placement: placement, ..input(1100, 100, active, 96) });
            after.retain_tab_viewport(&before, 100, active);
            assert_eq!(after.tabs[0].index, first);
            for (a, b) in after.tabs.iter().zip(&before.tabs) { assert_eq!((a.index, a.rect), (b.index, b.rect)); }
            let target = &after.tabs[2];
            assert_eq!(after.tab_drag_target(target.rect.x + 8, target.rect.y + target.rect.height / 2), Some(target.index));
            assert_eq!(after.tab_drag_target(after.address.x + 5, target.rect.y), None);
            let mut offscreen = ChromeLayout::new(&ChromeInput { tab_placement: placement, ..input(1100, 100, 0, 96) });
            offscreen.retain_tab_viewport(&after, 100, 0);
            assert_eq!(offscreen.tabs[0].index, 0);
        }
    }

    #[test]
    fn active_tab_stays_visible_when_tabs_overflow() {
        let l = ChromeLayout::new(&input(900, 40, 37, 96));
        assert!(l.tabs.len() < 40);
        assert!(l.tabs.iter().any(|t| t.index == 37));
        assert!(l.tabs.iter().all(|t| t.rect.width >= TAB_MIN_W));
    }

    #[test]
    fn hit_testing_distinguishes_tabs_close_and_caption() {
        let l = ChromeLayout::new(&input(1400, 2, 1, 96));
        let tab = &l.tabs[1];
        assert_eq!(l.hit(tab.rect.x + 4, tab.rect.y + 4), Hit::Tab(1));
        assert_eq!(l.hit(tab.close.x + 2, tab.close.y + 2), Hit::TabClose(1));
        assert_eq!(l.hit(l.new_tab.x + 2, l.new_tab.y + 2), Hit::NewTab);
        assert_eq!(l.hit(l.new_tab.right() + 20, 20), Hit::Caption);
        assert_eq!(l.hit(l.back.x + 1, l.back.y + 1), Hit::Back);
        let (id, r) = l.commands[0];
        assert_eq!(l.hit(r.x + 1, r.y + 1), Hit::Command(id));
        assert_eq!(l.hit(l.splitter.x + 1, l.splitter.y + 10), Hit::Splitter);
    }

    #[test]
    fn bookmarks_overflow_into_menu() {
        let widths = [120; 20];
        let l = ChromeLayout::new(&ChromeInput { bookmark_widths: &widths, ..input(1000, 1, 0, 96) });
        assert!(l.bookmarks.len() < 20);
        assert!(!l.bookmark_overflow.is_empty());
        assert!(l.bookmarks.last().unwrap().right() < l.bookmark_overflow.x);
    }

    #[test]
    fn tiny_window_has_no_negative_geometry() {
        let l = ChromeLayout::new(&ChromeInput { height: 0, ..input(0, 1, 0, 144) });
        for r in [l.address, l.search, l.browser, l.tree, l.status] { assert!(r.width >= 0 && r.height >= 0); }
    }

    #[test]
    fn hidden_tree_gives_full_width_browser() {
        let l = ChromeLayout::new(&ChromeInput { show_tree: false, show_bookmarks: false, ..input(1200, 1, 0, 96) });
        assert_eq!(l.browser.x, 0);
        assert!(l.tree.is_empty());
        assert_eq!(l.top, STRIP_H + NAV_H + COMMAND_H);
    }

    #[test]
    fn utility_buttons_follow_measured_caption_height_and_center() {
        for dpi in [96, 120, 144, 192] {
            let caption = Rect::new(1600 - scale(138, dpi), scale(3, dpi), scale(46, dpi), scale(30, dpi));
            let l = ChromeLayout::new(&ChromeInput { caption_button: Some(caption), ..input(1600, 2, 0, dpi) });
            assert_eq!(l.new_tab.y, l.tabs[0].rect.y);
            assert_eq!(l.new_tab.height, l.tabs[0].rect.height);
            for rect in [l.tab_list, l.app_menu] {
                assert_eq!(rect.y, caption.y);
                assert_eq!(rect.height, caption.height);
                assert_eq!(rect.y * 2 + rect.height, caption.y * 2 + caption.height);
                assert!(rect.right() < caption.x);
            }
        }
    }

    #[test]
    fn hiding_commands_reflows_buttons_and_removes_empty_groups() {
        let before = ChromeLayout::new(&input(1400, 2, 0, 96));
        let mut preferences = CommandBarPreferences::default();
        preferences.set_item_visible(CommandBarItem::Cut, false);
        let after = ChromeLayout::new(&ChromeInput { command_bar: &preferences, ..input(1400, 2, 0, 96) });
        assert!(after.command_rect(CommandId::Cut).is_none());
        assert_eq!(after.command_rect(CommandId::Copy).unwrap().x, before.command_rect(CommandId::Cut).unwrap().x);
        preferences.set_item_visible(CommandBarItem::New, false);
        let after = ChromeLayout::new(&ChromeInput { command_bar: &preferences, ..input(1400, 2, 0, 96) });
        assert_eq!(after.commands[0].0, CommandId::Copy);
        assert_eq!(after.commands[0].1.x, 12);
        assert!(after.separators.iter().all(|r| r.x > after.commands[0].1.right()));
    }

    #[test]
    fn hidden_command_bar_gives_its_entire_height_to_content() {
        for dpi in [96, 120, 144, 192] {
            let preferences = CommandBarPreferences { visible: false, ..Default::default() };
            let l = ChromeLayout::new(&ChromeInput { command_bar: &preferences, ..input(1600, 1, 0, dpi) });
            assert!(l.command_row.is_empty() && l.commands.is_empty() && l.separators.is_empty());
            assert_eq!(l.top, l.nav_row.bottom());
            assert_eq!(l.browser.y, l.nav_row.bottom());
        }
        let preferences = CommandBarPreferences { visible: true, hidden_items: CommandBarItem::ALL.to_vec() };
        let l = ChromeLayout::new(&ChromeInput { command_bar: &preferences, ..input(1400, 1, 0, 96) });
        assert!(l.command_row.is_empty());
    }

    #[test]
    fn narrow_command_rows_keep_every_action_without_overlap() {
        for dpi in [96, 120, 144, 192] {
            for width in [260, 360, 480, 600, 780, 1024, 1440] {
                let l = ChromeLayout::new(&input(width, 1, 0, dpi));
                assert_eq!(l.commands.len(), CommandBarItem::ALL.len());
                for (index, (_, rect)) in l.commands.iter().enumerate() {
                    assert!(rect.x >= 0 && rect.right() <= width);
                    assert!(rect.y >= l.command_row.y && rect.bottom() <= l.command_row.bottom());
                    for (_, other) in &l.commands[index + 1..] {
                        assert!(rect.right() <= other.x || other.right() <= rect.x || rect.bottom() <= other.y || other.bottom() <= rect.y,
                            "overlap at {width}px / {dpi} DPI: {rect:?}, {other:?}");
                    }
                }
                assert!(l.command_rect(CommandId::PreviewPane).is_some());
                assert!(l.command_rect(CommandId::DetailsPane).is_none());
            }
        }
        let narrow = ChromeLayout::new(&input(360, 1, 0, 96));
        assert!(narrow.compact_commands);
        assert!(narrow.command_row.height > COMMAND_H);
        let wide = ChromeLayout::new(&input(1440, 1, 0, 96));
        assert!(!wide.compact_commands);
        assert_eq!(wide.command_row.height, COMMAND_H);
    }

    #[test]
    fn sizes_use_explorer_units() {
        assert_eq!(format_size(512), "512 字节");
        assert_eq!(format_size(1536), "1.50 KB");
        assert_eq!(format_size(12 * 1024 * 1024), "12.0 MB");
        assert_eq!(format_size(300 * 1024 * 1024 * 1024), "300 GB");
    }
}
