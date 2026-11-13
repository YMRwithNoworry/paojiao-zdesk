use super::*;
use windows::Win32::Graphics::{Direct2D::ID2D1Bitmap, Imaging::IWICBitmapSource};
use zsdesk_ui::{chrome::{format_size, CommandId}, theme::glyph};
use zsdesk_windows::{gfx::{Align, Canvas, Color, RectF, TextStyle}, shell::{icons, pidl}};

/// Shell icon converted once to WIC and uploaded per render-target generation.
pub(super) struct IconEntry { wic: Option<IWICBitmapSource>, bitmap: Option<(u64, ID2D1Bitmap)> }

pub(super) fn c(color: zsdesk_ui::theme::Color) -> Color { Color::rgba8(color.r, color.g, color.b, color.a) }
fn rf(r: LayoutRect) -> RectF { RectF::new(r.x as f32, r.y as f32, r.width as f32, r.height as f32) }

pub(super) const UI_TEXT: f32 = 12.0;
const ICON_DIP: i32 = 16;

fn pixel_aligned_icon(rect: RectF) -> RectF {
    RectF::new(rect.x.round(), rect.y.round(), rect.w.round().max(1.0), rect.h.round().max(1.0))
}

impl WindowState {
    pub(super) fn sf(&self, v: f32) -> f32 { v * self.dpi as f32 / 96.0 }
    pub(super) fn ui_style(&self) -> TextStyle { TextStyle::ui(self.sf(UI_TEXT)) }

    /// Resolves (and caches) the small Shell icon for a location.
    fn icon_for(&mut self, location: &ShellLocation) -> Option<ID2D1Bitmap> {
        let px = self.s(ICON_DIP);
        let key = format!("{px}|{}", location.address());
        let gfx = self.gfx.as_ref()?;
        let generation = gfx.generation();
        if !self.icons.contains_key(&key) {
            let wic = resolve_icon(location, px).and_then(|icon| gfx.wic_from_icon(icon.0));
            if self.icons.len() > 256 { self.icons.clear(); }
            self.icons.insert(key.clone(), IconEntry { wic, bitmap: None });
        }
        let entry = self.icons.get_mut(&key)?;
        if entry.bitmap.as_ref().is_none_or(|(g, _)| *g != generation) {
            entry.bitmap = entry.wic.as_ref().and_then(|w| gfx.upload(w)).map(|b| (generation, b));
        }
        entry.bitmap.as_ref().map(|(_, b)| b.clone())
    }

    pub(super) fn paint(&mut self) {
        let (width, height) = self.client_size();
        if Surface::ensure(&mut self.surface, width, height).is_none() { return; }
        // Collect icons first: uploading needs `&mut self`, drawing only `&self`.
        let tab_icons: Vec<Option<ID2D1Bitmap>> = self.layout.tabs.clone().iter()
            .map(|slot| { let l = self.session.tabs[slot.index].current().clone(); self.icon_for(&l) }).collect();
        let current = self.current_location().clone();
        let address_icon = self.icon_for(&current);
        let bookmark_icons: Vec<Option<ID2D1Bitmap>> = (0..self.layout.bookmarks.len())
            .map(|i| {let index=self.bookmark_index(i)?;if self.session.bookmarks[index].is_folder{return None;}let t=self.session.bookmarks[index].target.clone();self.icon_for(&t)}).collect();
        let (Some(gfx), Some(surface)) = (self.gfx.as_ref(), self.surface.as_ref()) else { return };
        let result = gfx.draw(surface, |cv| {
            self.paint_strip(cv, &tab_icons);
            self.paint_nav(cv, address_icon.as_ref());
            self.paint_commands(cv);
            self.paint_bookmarks(cv, &bookmark_icons);
            self.paint_search_panel(cv);
            self.paint_content(cv);
            self.paint_status(cv);
        });
        if result.is_err() { self.icons.values_mut().for_each(|e| e.bitmap = None); }
    }

    fn button_fill(&self, cv: &Canvas, rect: LayoutRect, hit: Hit, radius: f32) {
        if rect.is_empty() { return; }
        let p = &self.palette;
        if self.pressed == Some(hit) && self.hover == hit { cv.round(rf(rect), radius, c(p.pressed)); }
        else if self.hover == hit { cv.round(rf(rect), radius, c(p.hover)); }
    }

    fn paint_chrome_layer(&self, cv: &Canvas, rect: RectF) {
        if crate::edition::THEMED && self.mica {
            cv.clear_rect(rect);
            cv.fill(rect, c(self.palette.layer).with_alpha(0.18));
        } else { cv.fill(rect, c(self.palette.layer)); }
    }

    fn paint_strip(&self, cv: &Canvas, icons: &[Option<ID2D1Bitmap>]) {
        let p = &self.palette;
        let strip = rf(self.layout.strip);
        // With a system material, transparent pixels expose the DWM backdrop; otherwise use the
        // Windows 11 tab-strip tint. DWM draws the caption buttons above either.
        cv.clear_rect(strip);
        // Fullscreen has no extended DWM frame: transparent pixels otherwise
        // become black and hide the dark caption glyphs.
        if !self.mica || window_actions::fullscreen(self.hwnd) { cv.fill(strip, if self.active { c(p.strip) } else { c(p.layer) }); }
        let text = if self.active { c(p.text) } else { c(p.text_secondary) };
        let active = self.session.active_tab;
        let radius = self.sf(8.0);
        let foot = self.sf(6.0);
        if self.layout.vertical_tabs{
            cv.fill(rf(self.layout.tab_panel),c(p.strip));
            let edge = if self.layout.nav_row.x > 0 { self.layout.tab_panel.right() as f32 - 0.5 } else { self.layout.tab_panel.x as f32 + 0.5 };
            cv.line(edge, self.layout.tab_panel.y as f32, edge, self.layout.tab_panel.bottom() as f32, c(p.divider), 1.0);
            let title=self.session.current_location().title();
            cv.text(&title,RectF::new(self.sf(14.0),strip.y,(self.layout.tab_list.x as f32-self.sf(24.0)).max(0.0),strip.h),&self.ui_style(),text);
        }
        for (n, slot) in self.layout.tabs.iter().enumerate() {
            let r = rf(slot.rect);
            if self.layout.vertical_tabs { cv.push_clip(r); }
            let is_active = slot.index == active;
            let hovered = matches!(self.hover, Hit::Tab(i) | Hit::TabClose(i) if i == slot.index);
            if is_active {
                if self.layout.vertical_tabs{cv.round(r,self.sf(6.0),c(p.layer));}else{cv.tab_shape(r, radius, foot, c(p.layer));}
            } else if hovered {
                cv.round(RectF::new(r.x + self.sf(2.0), r.y + self.sf(2.0), r.w - self.sf(4.0), r.h - self.sf(6.0)), self.sf(6.0), c(p.hover));
            } else {
                // Separator unless the neighbour is active or hovered.
                let next = self.layout.tabs.get(n + 1).map(|t| t.index);
                let next_busy = next.is_some_and(|i| i == active || matches!(self.hover, Hit::Tab(h) | Hit::TabClose(h) if h == i));
                if !self.layout.vertical_tabs && !next_busy && next.is_some() {
                    let x = r.right() - 0.5;
                    cv.line(x, r.y + self.sf(9.0), x, r.bottom() - self.sf(9.0), c(p.inactive_tab_separator), 1.0);
                }
            }
            let icon_size = self.sf(ICON_DIP as f32);
            let icon_rect = RectF::new(r.x + self.sf(12.0), r.y + (r.h - icon_size) / 2.0, icon_size, icon_size);
            if self.session.tabs[slot.index].locked&&!slot.show_close{cv.glyph("\u{E72E}",icon_rect,self.sf(14.0),text);}
            else if let Some(Some(bitmap)) = icons.get(n) { cv.image(bitmap, pixel_aligned_icon(icon_rect), 1.0); }
            else { cv.glyph(if matches!(self.session.tabs[slot.index].current(),ShellLocation::Search{..}){glyph::SEARCH}else{glyph::FOLDER}, icon_rect, self.sf(14.0), text); }
            let title_right = if slot.show_close { slot.close.x as f32 - self.sf(2.0) } else { r.right() - self.sf(10.0) };
            let title = self.session.tabs[slot.index].current().title();
            let title_x = icon_rect.right() + self.sf(8.0);
            cv.text(&title, RectF::new(title_x, r.y, (title_right - title_x).max(0.0), r.h), &self.ui_style(), text);
            if slot.show_close {
                let close = rf(slot.close);
                if self.hover == Hit::TabClose(slot.index) { cv.round(close, self.sf(4.0), c(p.hover)); }
                cv.glyph(if self.session.tabs[slot.index].locked{"\u{E72E}"}else{glyph::CLOSE_SMALL}, close, self.sf(10.0), text);
            }
            if self.layout.vertical_tabs { cv.pop_clip(); }
        }
        // New tab, tab list (count) and app menu buttons.
        if !self.layout.new_tab.is_empty() {
            self.button_fill(cv, self.layout.new_tab, Hit::NewTab, self.sf(4.0));
            let r=rf(self.layout.new_tab);
            if self.layout.vertical_tabs{
                cv.glyph(glyph::ADD,RectF::new(r.x,r.y,self.sf(32.0),r.h),self.sf(12.0),text);
                cv.text("新建标签页",RectF::new(r.x+self.sf(34.0),r.y,(r.w-self.sf(38.0)).max(0.0),r.h),&self.ui_style(),text);
            }else{cv.glyph(glyph::ADD,r,self.sf(12.0),text);}
        }
        if !self.layout.tab_list.is_empty() {
            self.button_fill(cv, self.layout.tab_list, Hit::TabList, self.sf(4.0));
            let r = rf(self.layout.tab_list);
            if self.session.ui.show_tab_count {
                let count=self.session.tabs.len();let label=count.to_string();
                let badge_w=self.sf((label.len()as f32*7.0+8.0).max(18.0)).min(r.w-self.sf(6.0));
                let badge = RectF::new(r.x+(r.w-badge_w)/2.0,r.y+(r.h-self.sf(18.0))/2.0,badge_w,self.sf(18.0));
                cv.stroke_round(badge, self.sf(3.0), text, self.sf(1.2).max(1.0));
                cv.text(&label, badge, &TextStyle::ui(self.sf(if label.len()>4{9.0}else{11.0})).weight(600).align(Align::Center), text);
            } else { cv.glyph("\u{E70D}",r,self.sf(12.0),text); }
        }
        if !self.layout.app_menu.is_empty() {
            self.button_fill(cv, self.layout.app_menu, Hit::AppMenu, self.sf(4.0));
            cv.glyph(glyph::MENU, rf(self.layout.app_menu), self.sf(14.0), text);
        }
        if !native_caption(self.hwnd) { self.paint_caption_buttons(cv, text); }
    }

    /// Windows 11 caption buttons drawn over the opaque strip (DWM still hit-tests them).
    fn paint_caption_buttons(&self, cv: &Canvas, text: Color) {
        let Some(buttons) = self.caption_buttons() else { return };
        let (hover, pressed) = caption_visual(self.hwnd);
        let maximized = unsafe { IsZoomed(self.hwnd) }.as_bool() || window_actions::fullscreen(self.hwnd);
        let glyphs = [("\u{E921}", HTMINBUTTON), (if maximized { "\u{E923}" } else { "\u{E922}" }, HTMAXBUTTON), ("\u{E8BB}", HTCLOSE)];
        for (rect, (g, code)) in buttons.iter().zip(glyphs) {
            let r = rf(*rect);
            let mut color = text;
            if hover == code {
                if code == HTCLOSE {
                    cv.fill(r, if pressed { Color::rgba8(0xC8, 0x47, 0x3A, 0xFF) } else { c(self.palette.danger) });
                    color = Color::rgba8(0xFF, 0xFF, 0xFF, 0xFF);
                } else {
                    cv.fill(r, if pressed { c(self.palette.pressed) } else { c(self.palette.hover) });
                }
            }
            cv.glyph(g, r, self.sf(10.0), color);
        }
    }

    fn paint_nav(&self, cv: &Canvas, address_icon: Option<&ID2D1Bitmap>) {
        let p = &self.palette;
        let l = &self.layout;
        self.paint_chrome_layer(cv, rf(LayoutRect::new(l.nav_row.x, l.nav_row.y, l.nav_row.width, l.top - l.nav_row.y)));
        let tab = self.session.active_tab();
        let busy = self.pending.is_some();
        let buttons = [
            (l.back, Hit::Back, glyph::BACK, tab.can_go_back() && !busy),
            (l.forward, Hit::Forward, glyph::FORWARD, tab.can_go_forward() && !busy),
            (l.up, Hit::Up, glyph::UP, !matches!(self.current_location(), ShellLocation::ThisPc) && !busy),
            (l.refresh, Hit::Refresh, glyph::REFRESH, true),
        ];
        for (rect, hit, g, enabled) in buttons {
            if enabled { self.button_fill(cv, rect, hit, self.sf(4.0)); }
            cv.glyph(g, rf(rect), self.sf(14.0), if enabled { c(p.text) } else { c(p.text_disabled) });
        }
        // Address box.
        let address = rf(l.address);
        let focused = self.address_editing;
        self.text_box(cv, address, focused, self.hover == Hit::Address && !focused);
        if !focused {
            let icon = self.sf(ICON_DIP as f32);
            let icon_rect = RectF::new(address.x + self.sf(10.0), address.y + (address.h - icon) / 2.0, icon, icon);
            if let Some(bitmap) = address_icon { cv.image(bitmap, pixel_aligned_icon(icon_rect), 1.0); }
            for slot in &self.crumb_slots {
                let r = rf(slot.rect);
                let hovered = self.hover_crumb == Some(slot.part);
                let open = self.open_chevron == Some(slot.part);
                if open { cv.round(r, self.sf(4.0), c(p.pressed)); } else if hovered { cv.round(r, self.sf(4.0), c(p.hover)); }
                match slot.part {
                    CrumbPart::Crumb(i) => {
                        let title = &self.crumbs[i].title;
                        cv.text(title, r, &self.ui_style().align(Align::Center), c(p.text));
                    }
                    CrumbPart::Chevron(_) => cv.glyph(if open { glyph::CHEVRON_DOWN } else { glyph::CHEVRON_RIGHT }, r, self.sf(9.0), c(p.text_secondary)),
                    CrumbPart::Overflow => cv.text("…", r, &self.ui_style().align(Align::Center), c(p.text)),
                }
            }
        }
        // Search box.
        {
            let search = rf(l.search);
            let search_focused = unsafe { GetFocus() } == self.search_edit;
            self.text_box(cv, search, search_focused, self.hover == Hit::Search && !search_focused);
            let glyph_rect = RectF::new(search.right() - self.sf(34.0), search.y, self.sf(30.0), search.h);
            cv.glyph(if self.search_panel.open { glyph::CLOSE_SMALL } else { glyph::SEARCH }, glyph_rect, self.sf(13.0), c(p.text_secondary));
        }
    }

    /// Windows 11 TextBox: rounded fill, hairline border and bottom stroke (accent when focused).
    fn text_box(&self, cv: &Canvas, r: RectF, focused: bool, hovered: bool) {
        let p = &self.palette;
        let radius = self.sf(4.0);
        let fill = if focused { c(p.input_focused) } else if hovered && p.mode == Mode::Light { Color::rgba8(0xF9, 0xF9, 0xF9, 0xFF) } else { c(p.input) };
        cv.round(r, radius, fill);
        cv.stroke_round(r, radius, c(p.input_border), 1.0);
        cv.push_clip(RectF::new(r.x, r.bottom() - radius, r.w, radius));
        if focused {
            cv.push_clip(RectF::new(r.x, r.bottom() - self.sf(2.0), r.w, self.sf(2.0)));
            cv.round(r, radius, c(p.accent));
            cv.pop_clip();
        } else {
            cv.push_clip(RectF::new(r.x, r.bottom() - 1.0, r.w, 1.0));
            cv.round(r, radius, c(p.input_underline).with_alpha(0.55));
            cv.pop_clip();
        }
        cv.pop_clip();
    }

    fn paint_commands(&self, cv: &Canvas) {
        let p = &self.palette;
        let l = &self.layout;
        let row = l.command_row;
        if row.is_empty() { return; }
        cv.line(row.x as f32, row.y as f32 + 0.5, row.right() as f32, row.y as f32 + 0.5, c(p.divider), 1.0);
        let selected = self.view_status.selected > 0;
        let can_paste = clipboard_has_files();
        for (id, rect) in &l.commands {
            let enabled = match id {
                CommandId::Cut | CommandId::Copy | CommandId::Rename | CommandId::Delete | CommandId::Share => selected,
                CommandId::Paste => can_paste,
                _ => true,
            };
            let hit = Hit::Command(*id);
            let toggled = (*id == CommandId::DetailsPane && self.session.ui.show_details) || (*id == CommandId::PreviewPane && self.session.ui.show_preview);
            if toggled { cv.round(rf(*rect), self.sf(4.0), c(p.pressed)); }
            if enabled { self.button_fill(cv, *rect, hit, self.sf(4.0)); }
            let color = if enabled { c(p.text) } else { c(p.text_disabled) };
            let r = rf(*rect);
            let g = match id {
                CommandId::New => glyph::ADD, CommandId::Cut => glyph::CUT, CommandId::Copy => glyph::COPY,
                CommandId::Paste => glyph::PASTE, CommandId::Rename => glyph::RENAME, CommandId::Share => glyph::SHARE,
                CommandId::Delete => glyph::DELETE, CommandId::Sort => glyph::SORT, CommandId::View => glyph::VIEW,
                CommandId::More => glyph::MORE, CommandId::DetailsPane => glyph::PROPERTIES, CommandId::PreviewPane => glyph::PREVIEW,
            };
            let glyph_color = if enabled && *id == CommandId::New { c(p.accent) } else { color };
            if l.compact_commands {
                cv.glyph(g, r, self.sf(15.0), glyph_color);
                continue;
            }
            match id.label() {
                None => cv.glyph(g, r, self.sf(15.0), glyph_color),
                Some(label) => {
                    let icon = RectF::new(r.x + self.sf(8.0), r.y, self.sf(20.0), r.h);
                    cv.glyph(g, icon, self.sf(15.0), glyph_color);
                    let right = if id.has_dropdown() { r.right() - self.sf(20.0) } else { r.right() - self.sf(6.0) };
                    let x = icon.right() + self.sf(6.0);
                    cv.text(label, RectF::new(x, r.y, right - x, r.h), &self.ui_style(), color);
                    if id.has_dropdown() {
                        cv.glyph(glyph::CHEVRON_DOWN, RectF::new(r.right() - self.sf(20.0), r.y, self.sf(14.0), r.h), self.sf(8.0), color);
                    }
                }
            }
        }
        for sep in &l.separators { cv.fill(rf(*sep), c(p.divider)); }
    }

    fn paint_bookmarks(&self, cv: &Canvas, icons: &[Option<ID2D1Bitmap>]) {
        let p = &self.palette;
        let l = &self.layout;
        if l.bookmarks_row.is_empty() { return; }
        self.paint_chrome_layer(cv, rf(l.bookmarks_row));
        if self.session.bookmarks.is_empty() {
            let r = rf(l.bookmarks_row);
            cv.text("将文件夹拖到这里，或按 Ctrl+D 收藏当前文件夹", RectF::new(r.x + self.sf(14.0), r.y, r.w - self.sf(28.0), r.h), &self.ui_style(), c(p.text_secondary));
        }
        for (i, rect) in l.bookmarks.iter().enumerate() {
            let Some(index)=self.bookmark_index(i)else{continue;};
            if self.rename_id == Some(self.session.bookmarks[index].id) {
                let r = rf(LayoutRect::new(rect.x, rect.y, rect.width.max(self.s(140)), rect.height));
                self.text_box(cv, r, true, false);
                continue;
            }
            self.button_fill(cv, *rect, Hit::Bookmark(i), self.sf(4.0));
            if let Some(drag)=self.bookmark_drag.filter(|drag|drag.moved){if self.hover==Hit::Bookmark(i){
                if self.session.bookmarks[index].is_folder&&drag.point.x>=rect.x+rect.width/4&&drag.point.x<rect.right()-rect.width/4{
                    cv.stroke_round(rf(*rect),self.sf(4.0),c(p.accent),self.sf(2.0));
                }else{let x=if drag.point.x<rect.x+rect.width/2{rect.x}else{rect.right()}as f32;cv.line(x,rect.y as f32,x,rect.bottom()as f32,c(p.accent),self.sf(2.0));}
            }}
            let r = rf(*rect);
            let icon = self.sf(ICON_DIP as f32);
            let icon_rect = RectF::new(r.x + self.sf(8.0), r.y + (r.h - icon) / 2.0, icon, icon);
            match icons.get(i) { Some(Some(b)) => cv.image(b, pixel_aligned_icon(icon_rect), 1.0), _ => cv.glyph(glyph::FOLDER, icon_rect, self.sf(14.0), c(p.text)) }
            let x = icon_rect.right() + self.sf(6.0);
            cv.text(&self.bookmark_label(index), RectF::new(x, r.y, r.right() - x - self.sf(6.0), r.h), &self.ui_style(), c(p.text));
        }
        if !l.bookmark_overflow.is_empty() {
            self.button_fill(cv, l.bookmark_overflow, Hit::BookmarkOverflow, self.sf(4.0));
            cv.glyph("\u{E76C}", rf(l.bookmark_overflow), self.sf(10.0), c(p.text));
        }
        if let Some(index)=self.renamed_bookmark_index(){if !self.bookmark_order.iter().take(l.bookmarks.len()).any(|i|*i==index){self.text_box(cv,rf(self.bookmark_rect(index)),true,false);}}
    }

    fn paint_content(&self, cv: &Canvas) {
        let p = &self.palette;
        let l = &self.layout;
        cv.line(l.nav_row.x as f32, l.top as f32 - 0.5, l.nav_row.right() as f32, l.top as f32 - 0.5, c(p.divider), 1.0);
        // Visible only around the native children (splitter, empty tree area while loading).
        let content = LayoutRect::new(l.nav_row.x, l.top, l.nav_row.width, (l.status.y - l.top).max(0));
        cv.fill(rf(content), c(p.content));
        if self.session.ui.dual_pane {
            for (index,header) in l.pane_headers.iter().enumerate() {
                let active=(index==1)==self.active_pane_right;
                cv.fill(rf(*header),c(if active {p.layer}else{p.content}));
                let title=self.pane_title(index==1);
                cv.text(&title,RectF::new((header.x+self.s(10))as f32,header.y as f32,
                    (header.width-self.s(20)).max(0)as f32,header.height as f32),&self.ui_style(),c(if active{p.text}else{p.text_secondary}));
                cv.line(header.x as f32,header.bottom()as f32-1.0,header.right()as f32,header.bottom()as f32-1.0,
                    c(if active{p.accent}else{p.divider}),if active{2.0}else{1.0});
            }
            // A hairline divider like Windows 11 split views; the full gap stays
            // the drag target and shows a grip while hovered or dragged.
            let split=rf(l.pane_splitter);
            cv.fill(split,c(p.content));
            let middle=(split.x+split.w/2.0).floor()+0.5;
            if self.hover==Hit::PaneSplitter || self.pane_splitter_drag.is_some() {
                let grip=self.sf(3.0);
                cv.round(RectF::new(middle-grip/2.0,split.y,grip,split.h),grip/2.0,c(p.text_secondary).with_alpha(0.55));
            } else {
                cv.line(middle,split.y,middle,split.bottom(),c(p.divider),1.0);
            }
        }
        if !l.splitter.is_empty() && (self.hover == Hit::Splitter || self.splitter_drag.is_some()) {
            let x = l.splitter.x as f32 + l.splitter.width as f32 / 2.0;
            cv.line(x, l.splitter.y as f32, x, l.splitter.bottom() as f32, c(p.divider), 1.0);
        }
    }

    fn paint_status(&self, cv: &Canvas) {
        let p = &self.palette;
        let l = &self.layout;
        if l.status.is_empty() {
            // Transient messages sit at the right end of the command bar.
            if self.status.is_empty() || l.command_row.is_empty() { return; }
            let row = rf(l.command_row);
            let panes = [CommandId::PreviewPane, CommandId::DetailsPane];
            let left = l.commands.iter().filter(|(id, _)| !panes.contains(id)).map(|(_, r)| r.right()).max().unwrap_or(0) as f32 + self.sf(16.0);
            let right = l.commands.iter().filter(|(id, _)| panes.contains(id)).map(|(_, r)| r.x).min().map_or(row.right() - self.sf(12.0), |x| x as f32 - self.sf(12.0));
            cv.text(&self.status, RectF::new(left, row.y, (right - left).max(0.0), row.h), &self.ui_style().align(Align::End),
                c(if status_is_error(&self.status) { p.danger } else { p.text_secondary }));
            return;
        }
        let r = rf(l.status);
        cv.fill(r, c(p.content));
        let mut text = format!("{} 个项目", self.view_status.items);
        if self.view_status.selected > 0 {
            text.push_str(&format!("    选中 {} 个项目", self.view_status.selected));
            if let Some(bytes) = self.view_status.selected_bytes { text.push_str(&format!("  {}", format_size(bytes))); }
        }
        let style = TextStyle::ui(self.sf(UI_TEXT - 0.5));
        let left = r.x + self.sf(12.0);
        let (w, _) = cv.gfx().measure(&text, &style, r.w);
        cv.text(&text, RectF::new(left, r.y, w + 2.0, r.h), &style, c(p.text_secondary));
        if !self.status.is_empty() || self.search_progress.is_some() {
            let x = left + w + self.sf(24.0);
            let text=if self.status.is_empty(){self.search_progress.as_deref().unwrap_or("")}else{&self.status};
            cv.text(text, RectF::new(x, r.y, (l.status_dual_pane.x as f32 - x - self.sf(8.0)).max(0.0), r.h), &style,
                c(if status_is_error(&self.status) {p.danger} else {p.text_secondary}));
        }
        let (mode, _) = self.view_mode;
        for (rect, hit, g, on) in [(l.status_dual_pane,Hit::StatusDualPane,glyph::DUAL_PANE,self.session.ui.dual_pane),
            (l.status_details, Hit::StatusDetails, glyph::DETAILS_VIEW, mode == 4), (l.status_tiles, Hit::StatusTiles, glyph::TILES_VIEW, mode == 1)] {
            if on { cv.round(rf(rect), self.sf(4.0), c(p.pressed)); cv.round(RectF::new(rect.x as f32 + self.sf(9.0), rect.bottom() as f32 - self.sf(3.0), rect.width as f32 - self.sf(18.0), self.sf(2.0)), self.sf(1.0), c(p.accent)); }
            self.button_fill(cv, rect, hit, self.sf(4.0));
            cv.glyph(g, rf(rect), self.sf(12.0), c(p.text));
        }
    }

    /// Lays out the breadcrumb inside the address box (overflow collapses leading crumbs).
    pub(super) fn layout_crumbs(&mut self) {
        self.crumb_slots.clear();
        let Some(gfx) = self.gfx.as_ref() else { return };
        let a = self.layout.address;
        if a.is_empty() || self.crumbs.is_empty() { return; }
        let style = self.ui_style();
        let pad = self.s(6);
        let widths: Vec<i32> = self.crumbs.iter().map(|c| gfx.measure(&c.title, &style, 2000.0).0 as i32 + pad * 2).collect();
        self.crumb_slots = breadcrumb_geometry(a, &widths, self.dpi);
    }

    pub(super) fn crumb_at(&self, x: i32, y: i32) -> Option<CrumbPart> {
        if self.address_editing || !self.layout.address.contains(x, y) { return None; }
        self.crumb_slots.iter().find(|s| s.rect.contains(x, y)).map(|s| s.part)
    }

    pub(super) fn measure_bookmarks(&mut self) {
        self.bookmark_order=self.session.visible_bookmarks();
        let Some(gfx) = self.gfx.as_ref() else { return };
        let style = self.ui_style();
        let extra = self.s(8) + self.s(ICON_DIP) + self.s(6) + self.s(10);
        self.bookmark_widths = self.bookmark_order.iter().map(|&index|
            (gfx.measure(&self.bookmark_label(index), &style, 600.0).0 as i32 + extra).min(self.s(220))).collect();
    }
}

fn breadcrumb_geometry(address: LayoutRect, widths: &[i32], dpi: u32) -> Vec<CrumbSlot> {
    let s = |value| scale(value, dpi);
    let mut slots = Vec::new();
    if address.is_empty() || widths.is_empty() { return slots; }
    let mut x = address.x + s(10 + ICON_DIP + 4);
    let end = address.right() - s(8);
    let y = address.y + s(4);
    let height = (address.height - s(8)).max(0);
    if x >= end || height == 0 { return slots; }
    let chevron = s(18);
    let overflow = s(26);
    let mut first = 0;
    while first + 1 < widths.len()
        && widths[first..].iter().map(|width| i64::from(*width + chevron)).sum::<i64>()
            + if first > 0 { i64::from(overflow) } else { 0 } > i64::from(end - x) {
        first += 1;
    }
    if first > 0 && end - x >= overflow + s(24) {
        slots.push(CrumbSlot { part: CrumbPart::Overflow, rect: LayoutRect::new(x, y, overflow, height) });
        x += overflow;
    }
    for (index, desired) in widths.iter().enumerate().skip(first) {
        let remaining = end - x;
        if remaining <= 0 { break; }
        let arrow = if remaining >= s(24) + chevron { chevron } else { 0 };
        let width = (*desired).max(0).min(remaining - arrow);
        if width == 0 { break; }
        slots.push(CrumbSlot { part: CrumbPart::Crumb(index), rect: LayoutRect::new(x, y, width, height) });
        x += width;
        if arrow > 0 {
            slots.push(CrumbSlot { part: CrumbPart::Chevron(index), rect: LayoutRect::new(x, y, arrow, height) });
            x += arrow;
        }
    }
    slots
}

fn resolve_icon(location: &ShellLocation, px: i32) -> Option<icons::OwnedIcon> {
    if matches!(location,ShellLocation::Search{..}){return None;}
    // Network paths may be offline: resolve them from the name alone.
    if let ShellLocation::Path(path) = location {
        let text = path.to_string_lossy();
        if text.starts_with(r"\\") {
            let index = icons::icon_index_for_name(&text, path.extension().is_none())?;
            return icons::system_icon(index, px).map(icons::OwnedIcon);
        }
    }
    let pidl = pidl::location_pidl(location)?;
    icons::pidl_icon(&pidl, px)
}

fn clipboard_has_files() -> bool {
    use windows::Win32::System::{DataExchange::{IsClipboardFormatAvailable, RegisterClipboardFormatW}, Ole::CF_HDROP};
    if unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32) }.is_ok() { return true; }
    let shell = unsafe { RegisterClipboardFormatW(windows::core::w!("Shell IDList Array")) };
    shell != 0 && unsafe { IsClipboardFormatAvailable(shell) }.is_ok()
}

#[cfg(test)]
mod icon_geometry_tests {
    use super::*;
    #[test]
    fn bitmap_icons_keep_integer_pixel_edges_at_fractional_scale() {
        let rect = pixel_aligned_icon(RectF::new(12.5, 7.5, 20.0, 20.0));
        assert_eq!((rect.x, rect.y, rect.w, rect.h), (13.0, 8.0, 20.0, 20.0));
    }

    #[test]
    fn long_breadcrumbs_never_escape_narrow_or_offset_address_fields() {
        for dpi in [96, 120, 144, 192] { for offset in [0, 210] { for width in [30, 70, 120, 300] {
            let bounds = LayoutRect::new(scale(offset + 160, dpi), scale(80, dpi), scale(width, dpi), scale(32, dpi));
            let widths = [scale(90, dpi), scale(170, dpi), scale(700, dpi)];
            let slots = breadcrumb_geometry(bounds, &widths, dpi);
            for (index, slot) in slots.iter().enumerate() {
                assert!(slot.rect.x >= bounds.x && slot.rect.right() <= bounds.right());
                assert!(slot.rect.y >= bounds.y && slot.rect.bottom() <= bounds.bottom());
                assert!(!slot.rect.is_empty());
                if index > 0 { assert!(slots[index - 1].rect.right() <= slot.rect.x); }
            }
            if width >= 70 { assert!(slots.iter().any(|slot| slot.part == CrumbPart::Crumb(2))); }
        }}}
    }
}

/// Status text that reports a failure is drawn in the critical color; progress
/// and confirmations ("已复制 3 个路径") stay neutral.
fn status_is_error(text: &str) -> bool {
    ["失败", "无法", "错误", "未找到", "不支持", "未保存", "未更新", "未启动", "异常", "超时", "需要修复"].iter().any(|word| text.contains(word))
}

#[cfg(test)]
mod status_tests {
    #[test]
    fn confirmations_are_not_painted_as_errors() {
        assert!(!super::status_is_error("已复制 3 个路径"));
        assert!(!super::status_is_error("下次登录 Windows 时将启动桌面格子"));
        assert!(super::status_is_error("操作失败：拒绝访问"));
        assert!(super::status_is_error("无法预览：文件被占用"));
    }
}
