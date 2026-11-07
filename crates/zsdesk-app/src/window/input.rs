use super::*;
use zsdesk_ui::{chrome::CommandId, menus::{self, MenuItemSpec, MenuSpec, ShowState, SortKey, UiCommand}};
use zsdesk_windows::shell::commands::{folder_settings, set_folder_settings, Column};

impl WindowState {
    pub(super) unsafe fn rebuild_tabs(&mut self) {
        self.relayout();
        unsafe { self.update_address(); }
    }

    pub(super) unsafe fn action(&mut self, id: usize, _value: isize) {
        match id {
            ACTION_COMMIT_ADDRESS => unsafe { self.commit_address() },
            ACTION_CANCEL_ADDRESS => { self.end_address_edit(); self.focus_view(); }
            ACTION_END_ADDRESS_EDIT => if unsafe { GetFocus() } != self.address_edit { self.end_address_edit(); },
            ACTION_COMMIT_RENAME => unsafe { self.commit_bookmark_rename() },
            ACTION_CANCEL_RENAME => unsafe { self.cancel_rename() },
            ACTION_SEARCH => unsafe { self.search() },
            ACTION_FOCUS_VIEW => { self.end_address_edit(); self.focus_view(); }
            _ => if let Some(command) = Command::from_id(id as u16) { unsafe { self.execute(command); } },
        }
    }

    fn focus_view(&self) { if let Some(browser) = &self.browser { unsafe { browser.focus(); } } }

    unsafe fn execute(&mut self, command: Command) {
        match command {
            Command::NewTab => unsafe { self.new_tab(ShellLocation::ThisPc) },
            Command::CloseTab => unsafe { self.close_tab(self.session.active_tab) },
            Command::ReopenTab => unsafe { self.reopen_tab() },
            Command::NextTab | Command::PreviousTab => {
                let delta = if command == Command::NextTab { 1 } else { -1 };
                let next = (self.session.active_tab as isize + delta).rem_euclid(self.session.tabs.len() as isize) as usize;
                unsafe { self.select_tab(next); }
            }
            Command::SelectTab(n) => {
                let index = if n == 9 { self.session.tabs.len() - 1 } else { (n as usize).saturating_sub(1) };
                unsafe { self.select_tab(index); }
            }
            Command::Back => unsafe { self.travel(true) },
            Command::Forward => unsafe { self.travel(false) },
            Command::Up => unsafe { self.go_up() },
            Command::FocusAddress => unsafe { self.focus_address() },
            Command::Refresh => unsafe {
                if matches!(self.current_location(),ShellLocation::Search{scope:None,..}){if let Some(engine)=&self.fast_search{engine.cancel();engine.refresh();}}
                self.capture_active(); self.navigate_current();
            },
            Command::AddBookmark => self.add_current_bookmark(),
            Command::FocusSearch => {
                let _ = unsafe { SetFocus(Some(self.search_edit)) };
                unsafe { SendMessageW(self.search_edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1))); }
            }
            Command::NewFolder => self.shell_result(self.browser.as_ref().map(|b| b.invoke_background(self.hwnd, "NewFolder"))),
            Command::CopyPath => self.copy_paths(),
            Command::TogglePreview => { let d = self.session.ui.show_details; self.set_panes(!self.session.ui.show_preview, d && self.session.ui.show_preview); }
            Command::ToggleDetails => { let p = self.session.ui.show_preview; self.set_panes(p && self.session.ui.show_details, !self.session.ui.show_details); }
        }
    }

    pub(super) fn shell_result(&mut self, result: Option<windows::core::Result<()>>) {
        if let Some(Err(error)) = result {
            // ERROR_CANCELLED / no selection are not failures worth reporting.
            if ![0x800704C7u32, 0x80004005, 0x80070490].contains(&(error.code().0 as u32)) { self.show_message(format!("操作失败：{}", error.message())); }
        }
        self.refresh_status();
    }

    // ---- pointer --------------------------------------------------------

    pub(super) fn set_hover(&mut self, hit: Hit, crumb: Option<CrumbPart>) {
        if hit == self.hover && crumb == self.hover_crumb { return; }
        let old = self.hover;
        self.hover = hit;
        self.hover_crumb = crumb;
        self.update_tooltip();
        let rows = [self.layout.strip, self.layout.nav_row, self.layout.command_row, self.layout.bookmarks_row, self.layout.status];
        for row in rows {
            let touches = |h: Hit| self.row_of(h) == Some(row);
            if touches(old) || touches(hit) { self.invalidate_rect(row); }
        }
        if matches!(old, Hit::Splitter) || matches!(hit, Hit::Splitter) { self.invalidate_rect(self.layout.splitter); }
    }

    fn row_of(&self, hit: Hit) -> Option<LayoutRect> {
        Some(match hit {
            Hit::None | Hit::Splitter => return None,
            Hit::Caption | Hit::Tab(_) | Hit::TabClose(_) | Hit::NewTab | Hit::TabList | Hit::AppMenu => self.layout.strip,
            Hit::Back | Hit::Forward | Hit::Up | Hit::Refresh | Hit::Address | Hit::Search => self.layout.nav_row,
            Hit::Command(_) => self.layout.command_row,
            Hit::Bookmark(_) | Hit::BookmarkOverflow | Hit::BookmarksBar => self.layout.bookmarks_row,
            Hit::StatusDetails | Hit::StatusTiles | Hit::Status => self.layout.status,
        })
    }

    fn update_tooltip(&mut self) {
        if self.tooltip.0.is_null() { return; }
        let tab_title = |i: usize| self.session.tabs.get(i).map(|t| t.current().address()).unwrap_or_default();
        let (text, rect): (String, LayoutRect) = match self.hover {
            Hit::Tab(i) => (tab_title(i), self.layout.tabs.iter().find(|t| t.index == i).map(|t| t.rect).unwrap_or_default()),
            Hit::TabClose(i) => ("关闭标签页 (Ctrl+W)".into(), self.layout.tabs.iter().find(|t| t.index == i).map(|t| t.close).unwrap_or_default()),
            Hit::NewTab => ("新建标签页 (Ctrl+T)".into(), self.layout.new_tab),
            Hit::TabList => (format!("所有标签页（{} 个）", self.session.tabs.len()), self.layout.tab_list),
            Hit::AppMenu => ("菜单".into(), self.layout.app_menu),
            Hit::Back => ("后退 (Alt+左箭头)".into(), self.layout.back),
            Hit::Forward => ("前进 (Alt+右箭头)".into(), self.layout.forward),
            Hit::Up => ("上移到上一级 (Alt+上箭头)".into(), self.layout.up),
            Hit::Refresh => ("刷新 (F5)".into(), self.layout.refresh),
            Hit::Command(id) => (id.tooltip().into(), self.layout.command_rect(id).unwrap_or_default()),
            Hit::Bookmark(i) => (self.session.bookmarks.get(i).map(|b| b.target.address()).unwrap_or_default(), self.layout.bookmarks.get(i).copied().unwrap_or_default()),
            Hit::StatusDetails => ("在窗口中显示每一项的相关信息 (Ctrl+Shift+6)".into(), self.layout.status_details),
            Hit::StatusTiles => ("使用大缩略图显示项 (Ctrl+Shift+2)".into(), self.layout.status_tiles),
            _ => (String::new(), LayoutRect::default()),
        };
        self.tooltip_text = wide(&text);
        unsafe { SendMessageW(self.tooltip, TTM_POP, None, None); }
        let info = TTTOOLINFOW { cbSize: size_of::<TTTOOLINFOW>() as u32, uFlags: TTF_SUBCLASS, hwnd: self.hwnd, uId: 1,
            rect: win_rect(rect), lpszText: PWSTR(self.tooltip_text.as_mut_ptr()), ..Default::default() };
        unsafe {
            SendMessageW(self.tooltip, TTM_NEWTOOLRECTW, None, Some(LPARAM(&info as *const TTTOOLINFOW as isize)));
            SendMessageW(self.tooltip, TTM_UPDATETIPTEXTW, None, Some(LPARAM(&info as *const TTTOOLINFOW as isize)));
        }
    }

    pub(super) fn mouse_move(&mut self, p: POINT, _keys: usize) {
        if !self.tracking_mouse {
            let mut track = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
            self.tracking_mouse = unsafe { TrackMouseEvent(&mut track) }.is_ok();
        }
        if let Some((start_x, start_w)) = self.splitter_drag {
            let width = (start_w + p.x - start_x).max(self.s(zsdesk_ui::chrome::MIN_TREE_W));
            let dip = (width as i64 * 96 / self.dpi as i64) as i32;
            if dip != self.session.ui.tree_width {
                self.session.ui.tree_width = dip.clamp(120, 800);
                self.relayout();
                self.position_children();
            }
            return;
        }
        if let Some(mut drag) = self.tab_drag {
            let threshold = unsafe { GetSystemMetricsForDpi(SM_CXDRAG, self.dpi) };
            if !drag.moved && (p.x - drag.start_x).abs() > threshold { drag.moved = true; }
            if drag.moved {
                if let Some(target) = self.layout.tab_index_at(p.x).filter(|&t| t != drag.index && t < self.session.tabs.len()) {
                    let tab = self.session.tabs.remove(drag.index);
                    self.session.tabs.insert(target, tab);
                    self.session.active_tab = target;
                    drag.index = target;
                    self.relayout();
                    self.invalidate_rect(self.layout.strip);
                }
            }
            self.tab_drag = Some(drag);
            return;
        }
        let hit = self.layout.hit(p.x, p.y);
        let crumb = if hit == Hit::Address { self.crumb_at(p.x, p.y) } else { None };
        self.set_hover(hit, crumb);
    }

    pub(super) fn mouse_down(&mut self, p: POINT) {
        let hit = self.layout.hit(p.x, p.y);
        self.pressed = Some(hit);
        match hit {
            Hit::Tab(i) => {
                unsafe { self.select_tab(i); }
                self.tab_drag = Some(TabDrag { index: self.session.active_tab, start_x: p.x, moved: false });
                unsafe { SetCapture(self.hwnd); }
            }
            Hit::Splitter => { self.splitter_drag = Some((p.x, self.layout.tree.width)); unsafe { SetCapture(self.hwnd); } }
            Hit::Address if self.crumb_at(p.x, p.y).is_none() && !self.address_editing => { self.pressed = None; unsafe { self.focus_address(); } return; }
            Hit::Search => { self.pressed = None; let _ = unsafe { SetFocus(Some(self.search_edit)) }; return; }
            Hit::None | Hit::Status | Hit::BookmarksBar => {}
            _ => unsafe { let _ = SetCapture(self.hwnd); },
        }
        if let Some(row) = self.row_of(hit) { self.invalidate_rect(row); }
    }

    pub(super) fn mouse_up(&mut self, p: POINT) {
        let pressed = self.pressed.take();
        let had_capture = unsafe { GetCapture() } == self.hwnd;
        if self.splitter_drag.take().is_some() { if had_capture { let _ = unsafe { ReleaseCapture() }; } self.save(); self.invalidate(); return; }
        if let Some(drag) = self.tab_drag.take() {
            if had_capture { let _ = unsafe { ReleaseCapture() }; }
            if drag.moved { self.save(); }
            return;
        }
        if had_capture { let _ = unsafe { ReleaseCapture() }; }
        let hit = self.layout.hit(p.x, p.y);
        if let Some(row) = self.row_of(hit) { self.invalidate_rect(row); }
        if pressed != Some(hit) { return; }
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        match hit {
            Hit::TabClose(i) => unsafe { self.close_tab(i) },
            Hit::NewTab => unsafe { self.new_tab(ShellLocation::ThisPc) },
            Hit::TabList => unsafe { self.tab_list_menu() },
            Hit::AppMenu => unsafe { self.app_menu() },
            Hit::Back => unsafe { self.execute(Command::Back) },
            Hit::Forward => unsafe { self.execute(Command::Forward) },
            Hit::Up => unsafe { self.execute(Command::Up) },
            Hit::Refresh => unsafe { self.execute(Command::Refresh) },
            Hit::Address => if let Some(part) = self.crumb_at(p.x, p.y) { unsafe { self.crumb_click(part, ctrl); } },
            Hit::Command(id) => unsafe { self.command(id) },
            Hit::Bookmark(i) => unsafe { self.open_bookmark(i, ctrl) },
            Hit::BookmarkOverflow => unsafe { self.bookmarks_overflow() },
            Hit::StatusDetails => self.set_view(menus::view_choice("details")),
            Hit::StatusTiles => self.set_view(menus::view_choice("large")),
            _ => {}
        }
    }

    pub(super) fn middle_click(&mut self, p: POINT) {
        match self.layout.hit(p.x, p.y) {
            Hit::Tab(i) | Hit::TabClose(i) => unsafe { self.close_tab(i) },
            Hit::Bookmark(i) => unsafe { self.open_bookmark(i, true) },
            Hit::Address => if let Some(CrumbPart::Crumb(i)) = self.crumb_at(p.x, p.y) {
                let location = self.crumbs[i].location.clone();
                unsafe { self.new_tab(location); }
            },
            _ => {}
        }
    }

    pub(super) fn right_click(&mut self, p: POINT) {
        match self.layout.hit(p.x, p.y) {
            Hit::Tab(i) | Hit::TabClose(i) => unsafe { self.tab_menu(i) },
            Hit::Bookmark(i) => unsafe { self.bookmark_menu(i) },
            Hit::BookmarksBar | Hit::BookmarkOverflow => unsafe { self.bookmarks_bar_menu() },
            Hit::Address => {
                let target = match self.crumb_at(p.x, p.y) { Some(CrumbPart::Crumb(i)) => self.crumbs[i].location.clone(), _ => self.current_location().clone() };
                let spec = menus::menu(vec![
                    menus::item("在新标签页中打开", "open-tab"),
                    menus::item("复制地址", "copy"),
                    menus::item("编辑地址", "edit"),
                ]);
                match popup(self.hwnd, &spec, None).as_deref() {
                    Some("open-tab") => unsafe { self.new_tab(target) },
                    Some("copy") => set_clipboard_text(self.hwnd, &target.address()),
                    Some("edit") => unsafe { self.focus_address() },
                    _ => {}
                }
            }
            _ => {}
        }
    }

    unsafe fn crumb_click(&mut self, part: CrumbPart, new_tab: bool) {
        match part {
            CrumbPart::Crumb(i) => {
                let location = self.crumbs[i].location.clone();
                if new_tab { unsafe { self.new_tab(location); } } else { unsafe { self.navigate_to(location); } }
            }
            CrumbPart::Chevron(i) => {
                let rect = self.crumb_slots.iter().find(|s| s.part == part).map(|s| s.rect);
                self.open_chevron = Some(part);
                self.invalidate_rect(self.layout.nav_row);
                let children = unsafe { zsdesk_windows::shell::child_folders(&self.crumbs[i].location, 400) };
                let items: Vec<MenuItemSpec> = if children.is_empty() {
                    vec![menus::entry("（没有子文件夹）", "none", false, false)]
                } else {
                    children.iter().enumerate().map(|(n, c)| menus::item(&c.title.replace('&', "&&"), &format!("c{n}"))).collect()
                };
                let choice = popup(self.hwnd, &menus::menu(items), rect);
                self.open_chevron = None;
                self.invalidate_rect(self.layout.nav_row);
                if let Some(n) = choice.and_then(|c| c.strip_prefix('c').and_then(|n| n.parse::<usize>().ok())) {
                    if let Some(child) = children.get(n) { unsafe { self.navigate_to(child.location.clone()); } }
                }
            }
            CrumbPart::Overflow => {
                let shown = self.crumb_slots.iter().find_map(|s| match s.part { CrumbPart::Crumb(i) => Some(i), _ => None }).unwrap_or(0);
                let items: Vec<MenuItemSpec> = self.crumbs[..shown].iter().enumerate().rev()
                    .map(|(n, c)| menus::item(&c.title.replace('&', "&&"), &format!("c{n}"))).collect();
                let rect = self.crumb_slots.first().map(|s| s.rect);
                if let Some(n) = popup(self.hwnd, &menus::menu(items), rect).and_then(|c| c.strip_prefix('c').and_then(|n| n.parse::<usize>().ok())) {
                    let location = self.crumbs[n].location.clone();
                    unsafe { self.navigate_to(location); }
                }
            }
        }
    }

    // ---- command bar ----------------------------------------------------

    unsafe fn command(&mut self, id: CommandId) {
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        let hwnd = self.hwnd;
        match id {
            CommandId::New => {
                let rect = self.layout.command_rect(id).unwrap_or_default();
                let point = self.screen_point(rect.x, rect.bottom());
                let result = self.browser.as_ref().map(|b| b.show_new_menu(hwnd, point));
                self.shell_result(result);
            }
            CommandId::Cut => { let r = self.browser.as_ref().map(|b| b.invoke_selection(hwnd, "cut", false)); self.shell_result(r); }
            CommandId::Copy => { let r = self.browser.as_ref().map(|b| b.invoke_selection(hwnd, "copy", false)); self.shell_result(r); }
            CommandId::Paste => { let r = self.browser.as_ref().map(|b| b.invoke_background(hwnd, "paste")); self.shell_result(r); }
            CommandId::Rename => { let r = self.browser.as_ref().map(|b| b.rename_selection()); self.shell_result(r); }
            CommandId::Share => { let r = self.browser.as_ref().map(|b| b.invoke_selection(hwnd, "Windows.ModernShare", false)); self.shell_result(r); }
            CommandId::Delete => { let r = self.browser.as_ref().map(|b| b.invoke_selection(hwnd, "delete", shift)); self.shell_result(r); }
            CommandId::Sort => unsafe { self.sort_menu() },
            CommandId::View => unsafe { self.view_menu() },
            CommandId::More => unsafe { self.more_menu() },
            CommandId::DetailsPane => unsafe { self.execute(Command::ToggleDetails) },
            CommandId::PreviewPane => unsafe { self.execute(Command::TogglePreview) },
        }
        self.focus_view();
    }

    fn screen_point(&self, x: i32, y: i32) -> POINT {
        let mut p = POINT { x, y };
        let _ = unsafe { ClientToScreen(self.hwnd, &mut p) };
        p
    }

    fn set_panes(&mut self, preview: bool, details: bool) {
        self.session.ui.show_preview = preview;
        self.session.ui.show_details = details;
        self.save();
        // IExplorerBrowser reads pane visibility only while initializing, so the
        // view is recreated and returns to the same folder, selection and layout.
        unsafe { self.capture_active(); }
        self.cancel_fast_search();self.browser = None;self.browser_role=BrowserRole::Fresh;
        self.pending = None;
        self.queued = None;
        self.shown_tab = None;
        let panes = Panes { preview, details };
        match unsafe { ExplorerBrowserHost::create(self.hwnd, win_rect(self.layout.browser), self.modern, panes) } {
            Ok(browser) => {browser.set_context_menu_options(crate::context_preferences::native(&self.session.ui.context_menu));self.browser = Some(browser);},
            Err(error) => self.show_message(format!("无法切换窗格：{error}")),
        }
        self.theme_children();
        unsafe { self.navigate_current(); }
        self.invalidate_rect(self.layout.command_row);
    }

    fn set_view(&mut self, choice: Option<menus::ViewChoice>) {
        let Some(choice) = choice else { return };
        if let Some(browser) = &self.browser {
            if let Err(err) = browser.set_view_mode(choice.mode, choice.icon_size) { self.show_message(format!("视图切换失败：{err}")); }
            else { unsafe { self.capture_active(); } self.save(); }
        }
        self.refresh_status();
    }

    unsafe fn view_menu(&mut self) {
        let (mode, icon) = self.browser.as_ref().and_then(|b| b.view_mode()).unwrap_or((0, 0));
        let (hidden, extensions) = folder_settings();
        let show = ShowState {
            navigation: self.session.ui.show_tree, preview: self.session.ui.show_preview, details: self.session.ui.show_details,
            bookmarks: self.session.ui.show_bookmarks, checkboxes: self.browser.as_ref().is_some_and(|b| b.checkboxes()),
            extensions, hidden,
        };
        let anchor = self.layout.command_rect(CommandId::View);
        let choice = popup(self.hwnd, &menus::view_menu(mode, icon, show), anchor);
        match choice.as_deref() {
            Some("show-nav") => { self.session.ui.show_tree = !show.navigation; self.save(); unsafe { self.resize(); } }
            Some("show-preview") => self.set_panes(!show.preview, false),
            Some("show-details") => self.set_panes(false, !show.details),
            Some("show-bookmarks") => { self.session.ui.show_bookmarks = !show.bookmarks; self.save(); unsafe { self.resize(); } }
            Some("show-checkboxes") => { if let Some(b) = &self.browser { let _ = b.set_checkboxes(!show.checkboxes); } }
            Some("show-extensions") => { set_folder_settings(None, Some(!show.extensions)); unsafe { self.execute(Command::Refresh); } }
            Some("show-hidden") => { set_folder_settings(Some(!show.hidden), None); unsafe { self.execute(Command::Refresh); } }
            Some(id) => self.set_view(menus::view_choice(id)),
            None => {}
        }
    }

    unsafe fn sort_menu(&mut self) {
        let to_key = |c: Column| match c { Column::Name => SortKey::Name, Column::Modified => SortKey::Modified, Column::Type => SortKey::Type, Column::Size => SortKey::Size, Column::Created => SortKey::Created };
        let to_column = |k: SortKey| match k { SortKey::Name => Column::Name, SortKey::Modified => Column::Modified, SortKey::Type => Column::Type, SortKey::Size => Column::Size, SortKey::Created => Column::Created };
        let Some(browser) = &self.browser else { return };
        let current = browser.sort();
        let group = browser.group().map(to_key);
        let anchor = self.layout.command_rect(CommandId::Sort);
        let choice = popup(self.hwnd, &menus::sort_menu(current.map(|(c, a)| (to_key(c), a)), group), anchor);
        let Some(browser) = &self.browser else { return };
        let (column, ascending) = current.unwrap_or((Column::Name, true));
        let result = match choice.as_deref() {
            Some("sort-asc") => browser.set_sort(column, true),
            Some("sort-desc") => browser.set_sort(column, false),
            Some("group-none") => browser.set_group(None),
            Some(id) if id.starts_with("group-") => browser.set_group(menus::sort_key(id).map(to_column)),
            Some(id) => match menus::sort_key(id) { Some(key) => browser.set_sort(to_column(key), ascending), None => Ok(()) },
            None => Ok(()),
        };
        self.shell_result(Some(result));
    }

    unsafe fn more_menu(&mut self) {
        let anchor = self.layout.command_rect(CommandId::More);
        let choice = popup(self.hwnd, &menus::more_menu(self.view_status.selected > 0, crate::edition::NETWORK), anchor);
        let hwnd = self.hwnd;
        match choice.as_deref() {
            Some("select-all") => if let Some(b) = &self.browser { b.select_all(true) },
            Some("select-none") => if let Some(b) = &self.browser { b.select_all(false) },
            Some("select-invert") => if let Some(b) = &self.browser { b.invert_selection() },
            Some("copy-path") => self.copy_paths(),
            Some("terminal") => self.open_terminal(),
            Some("properties") => {
                let r = self.browser.as_ref().map(|b| if self.view_status.selected > 0 { b.invoke_selection(hwnd, "properties", false) } else { b.invoke_background(hwnd, "properties") });
                self.shell_result(r);
            }
            Some("folder-options") => open_folder_options(),
            Some("map-drive") if crate::edition::NETWORK => {
                use windows::Win32::NetworkManagement::WNet::{WNetConnectionDialog, RESOURCETYPE_DISK};
                let _ = unsafe { WNetConnectionDialog(hwnd, RESOURCETYPE_DISK.0) };
                if matches!(self.current_location(), ShellLocation::ThisPc) { unsafe { self.execute(Command::Refresh); } }
            }
            Some("disconnect-drive") if crate::edition::NETWORK => {
                use windows::Win32::NetworkManagement::WNet::{WNetDisconnectDialog, RESOURCETYPE_DISK};
                let _ = unsafe { WNetDisconnectDialog(Some(hwnd), RESOURCETYPE_DISK.0) };
                if matches!(self.current_location(), ShellLocation::ThisPc) { unsafe { self.execute(Command::Refresh); } }
            }
            Some("add-netplace") if crate::edition::NETWORK => {
                // System wizard for network locations (SMB shares, FTP sites, WebDAV).
                if std::process::Command::new("rundll32.exe").args(["shwebsvc.dll,AddNetPlaceRunDll"]).spawn().is_err() {
                    self.show_message("无法打开“添加网络位置”向导".into());
                }
            }
            _ => {}
        }
        self.refresh_status();
    }

    fn copy_paths(&mut self) {
        let mut paths = self.browser.as_ref().map(|b| b.selection_paths()).unwrap_or_default();
        if paths.is_empty() { paths.push(self.current_location().address()); }
        let text = paths.iter().map(|p| if p.contains(' ') { format!("\"{p}\"") } else { p.clone() }).collect::<Vec<_>>().join("\r\n");
        set_clipboard_text(self.hwnd, &text);
        self.show_message(format!("已复制 {} 个路径", paths.len()));
    }

    fn open_terminal(&mut self) {
        let Some(path) = self.current_location().as_path().map(PathBuf::from) else { self.show_message("此位置不是文件夹".into()); return };
        let wt = std::process::Command::new("wt.exe").arg("-d").arg(&path).spawn();
        if wt.is_err() {
            let _ = std::process::Command::new("cmd.exe").arg("/K").current_dir(&path).spawn();
        }
    }

    unsafe fn app_menu(&mut self) {
        use zsdesk_windows::integration::{self, Integration};
        let state = menus::AppMenuState {
            desktop_running: crate::edition::DESKTOP.then(desktop_host_running),
            integration: match integration::current() { Integration::Off => 0, Integration::ContextMenu => 1, Integration::Default => 2 },
            win_e_agent: zsdesk_windows::desktop::win_e_agent_command().is_some(),
        };
        let anchor = Some(self.layout.app_menu);
        let choice = popup(self.hwnd, &menus::app_menu(state), anchor);
        match choice.as_deref() {
            Some("new-tab") => unsafe { self.new_tab(ShellLocation::ThisPc) },
            Some("quick-search") => unsafe{self.execute(Command::FocusSearch)},
            Some("search-rebuild-index") => {
                if self.fast_search.is_none(){match crate::fast_search::FastSearch::new(self.config_path.with_file_name("search-index.sqlite"),self.hwnd,WM_FAST_SEARCH){Ok(engine)=>self.fast_search=Some(engine),Err(error)=>{self.show_message(format!("无法建立文件名索引：{error}"));return;}}}
                if let Some(engine)=&self.fast_search{engine.rebuild();}
                if matches!(self.current_location(),ShellLocation::Search{scope:None,..}){unsafe{self.navigate_current();}}
                else{self.show_message("已请求重建文件名索引，可在快速搜索中查看结果".into());}
            },
            Some("interface-settings") => {
                let spec=menus::interface_settings_menu(&self.session.ui);
                if let Some(id)=popup(self.hwnd,&spec,Some(self.layout.app_menu)){
                    if menus::apply_interface_setting(&mut self.session.ui,&id){
                        self.crumbs_for=None;
                        if id=="search-all-files" || id=="search-current-folder"{unsafe{SetTimer(Some(self.hwnd),TIMER_SEARCH,20,None);}}
                        self.save();self.measure_bookmarks();self.relayout();self.position_children();
                        unsafe{self.update_address();}self.invalidate();
                    }
                }
            },
            Some("context-menu-settings") => {
                let spec=menus::context_menu_settings_menu(&self.session.ui.context_menu);
                if let Some(id)=popup(self.hwnd,&spec,Some(self.layout.app_menu)){
                    if menus::apply_context_menu_setting(&mut self.session.ui.context_menu,&id){
                        self.modern=self.session.ui.context_menu.mode==zsdesk_core::model::explorer::ContextMenuMode::Windows11;
                        let options=crate::context_preferences::native(&self.session.ui.context_menu);
                        zsdesk_windows::shell::menu::set_context_menu_options(options.clone());
                        if let Some(browser)=&self.browser{browser.set_context_menu_options(options);}
                        self.save();
                    }
                }
            },
            Some("routing-settings") => self.show_routing_settings(),
            Some("desktop-start") => self.launch_desktop(&[]),
            Some("desktop-organize") => self.launch_desktop(&["--organize".into()]),
            Some("desktop-map") => match self.current_location().as_path() {
                Some(path) => { let path = path.to_string_lossy().into_owned(); self.launch_desktop(&["--map".into(), path]); }
                None => self.show_message("只能映射文件系统中的文件夹".into()),
            },
            Some("desktop-quit") => self.launch_desktop(&["--quit".into()]),
            Some("desktop-tray") => self.launch_desktop(&["--tray".into()]),
            Some("integration-win-e") => self.toggle_win_e_agent(),
            Some(id @ ("integration-off" | "integration-context" | "integration-default" | "integration-restore")) => self.set_integration(id),
            Some("bookmark") => self.add_current_bookmark(),
            Some("folder-options") => open_folder_options(),
            Some("shortcuts") => {
                let text = if crate::edition::DESKTOP { SHORTCUTS } else { SHORTCUTS.split("\n\n桌面格子").next().unwrap_or(SHORTCUTS) };
                let text = wide(text);
                unsafe { MessageBoxW(Some(self.hwnd), pcwstr(&text), PCWSTR(wide("键盘快捷键").as_ptr()), MB_OK | MB_ICONINFORMATION); }
            }
            Some("about") => {
                let mut parts = vec!["多标签文件管理器"];
                if crate::edition::NETWORK { parts.push("FTP / SSH 访问"); }
                if crate::edition::DESKTOP { parts.push("桌面格子"); }
                let text = wide(&format!("ZSDesk {} {}\n\nWindows 11 风格：{}。\n界面设计基于 ZSUI，内容区为系统 Shell 视图。", env!("CARGO_PKG_VERSION"), crate::edition::name(), parts.join("、")));
                unsafe { MessageBoxW(Some(self.hwnd), pcwstr(&text), PCWSTR(wide("关于 ZSDesk").as_ptr()), MB_OK | MB_ICONINFORMATION); }
            }
            _ => {}
        }
    }

    fn toggle_win_e_agent(&mut self) {
        use zsdesk_windows::desktop::{set_win_e_agent_autostart, win_e_agent_command};
        if win_e_agent_command().is_some() {
            if !set_win_e_agent_autostart(None) {
                self.show_message("无法移除 Win+E 登录启动项".into());
                return;
            }
            match crate::win_e_agent::stop() {
                Ok(_) => self.show_message("已关闭 Win+E 接管和登录启动项；此电脑仍可从 ZSDesk 菜单打开".into()),
                Err(error) => self.show_message(format!("已移除登录启动项，但停止当前 Agent 失败：{error}")),
            }
            return;
        }
        let text = wide("启用后，当前用户登录时会启动一个轻量后台进程拦截 Win+E，并在 ZSDesk 中打开“此电脑”。该进程只处理 Win+E；关闭 ZSDesk 窗口不会停止它。可随时在此关闭。继续？");
        if unsafe { MessageBoxW(Some(self.hwnd), pcwstr(&text), PCWSTR(wide("接管 Win+E").as_ptr()), MB_OKCANCEL | MB_ICONQUESTION) } != IDOK { return; }
        let Ok(exe) = std::env::current_exe() else { return };
        let startup = format!("\"{}\" --win-e-agent", exe.display());
        if !set_win_e_agent_autostart(Some(&startup)) {
            self.show_message("无法写入当前用户登录启动项".into());
            return;
        }
        let child = std::process::Command::new(&exe).arg("--win-e-agent").spawn();
        match child {
            Ok(mut child) => {
                std::thread::sleep(std::time::Duration::from_millis(200));
                if child.try_wait().ok().flatten().is_some() {
                    let _ = set_win_e_agent_autostart(None);
                    self.show_message("Win+E 后台钩子启动失败，已撤销登录启动项".into());
                } else {
                    self.show_message("Win+E 已接管；此电脑将在 ZSDesk 中打开".into());
                }
            }
            Err(error) => {
                let _ = set_win_e_agent_autostart(None);
                self.show_message(format!("无法启动 Win+E 后台进程，已撤销登录启动项：{error}"));
            }
        }
    }

    /// 规划 §6.1.8: per-user takeover with backup and one-click restore.
    fn set_integration(&mut self, id: &str) {
        use zsdesk_windows::integration::{self, Integration};
        let level = match id { "integration-context" => Integration::ContextMenu, "integration-default" => Integration::Default, _ => Integration::Off };
        if level == Integration::Default {
            let text = wide("双击文件夹、磁盘或桌面上的文件夹时将用 ZSDesk 打开（仅当前用户）。\n\n修改前会备份原有设置，可随时在 ≡ → 系统集成 → 恢复系统资源管理器 中还原。\n\n继续？");
            if unsafe { MessageBoxW(Some(self.hwnd), pcwstr(&text), PCWSTR(wide("默认用 ZSDesk 打开").as_ptr()), MB_OKCANCEL | MB_ICONQUESTION) } != IDOK { return; }
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let backup = self.config_path.with_file_name("takeover-backup.json");
        match integration::apply(level, &exe, &backup) {
            Ok(()) => {
                let message=match level {
                Integration::Off => "已恢复接管前的文件夹设置".into(),
                Integration::ContextMenu => "已添加右键菜单“使用 ZSDesk 打开”".into(),
                Integration::Default => "文件夹和磁盘将默认用 ZSDesk 打开".into(),
                };self.show_message(message);
                if let Err(error)=crate::routing_control::configure(&self.config_path,level==Integration::Default,self.hwnd){self.show_message(format!("文件夹设置已保存；文件位置转发未更新：{error}"));}
            },
            Err(error) => self.show_message(format!("系统集成失败，已回滚：{error}")),
        }
    }

    fn launch_desktop(&mut self, extra: &[String]) {
        let Ok(exe) = std::env::current_exe() else { return };
        let mut command = std::process::Command::new(exe);
        command.arg("--desktop").args(extra);
        if let Some(dir) = self.config_path.parent() { command.arg("--profile-dir").arg(dir); }
        if let Err(error) = command.spawn() { self.show_message(format!("无法启动桌面格子：{error}")); }
    }

    unsafe fn tab_list_menu(&mut self) {
        let titles: Vec<String> = self.session.tabs.iter().map(|t| t.current().title()).collect();
        let spec = menus::tab_list_menu(&titles, self.session.active_tab, !self.closed_tabs.is_empty());
        match popup(self.hwnd, &spec, Some(self.layout.tab_list)).as_deref() {
            Some("restore") => unsafe { self.reopen_tab() },
            Some(id) => if let Some(i) = id.strip_prefix("tab-").and_then(|n| n.parse().ok()) { unsafe { self.select_tab(i); } },
            None => {}
        }
    }

    unsafe fn tab_menu(&mut self, index: usize) {
        if index >= self.session.tabs.len() { return; }
        let spec = menus::tab_menu(self.session.tabs.len(), index, !self.closed_tabs.is_empty());
        match popup(self.hwnd, &spec, None).as_deref() {
            Some("new") => unsafe { self.new_tab(ShellLocation::ThisPc) },
            Some("duplicate") => { let location = self.session.tabs[index].current().clone(); unsafe { self.new_tab(location); } }
            Some("bookmark") => { let l = self.session.tabs[index].current().clone(); self.add_bookmark(l.title(), l); }
            Some("close") => unsafe { self.close_tab(index) },
            Some("restore") => unsafe { self.reopen_tab() },
            Some(action @ ("close-others" | "close-right")) => unsafe { self.close_group(index, action == "close-right") },
            _ => {}
        }
    }

    unsafe fn close_group(&mut self, index: usize, right_only: bool) {
        let keep = self.session.tabs[index].id;
        let old = self.session.active_tab().id;
        unsafe { self.capture_active(); }
        let mut position = 0;
        self.session.tabs.retain(|tab| {
            let remove = tab.id != keep && (!right_only || position > index);
            position += 1;
            if remove { self.closed_tabs.push(tab.clone()); }
            !remove
        });
        let excess = self.closed_tabs.len().saturating_sub(20);
        self.closed_tabs.drain(..excess);
        self.session.active_tab = self.session.tabs.iter().position(|t| t.id == old)
            .unwrap_or_else(|| self.session.tabs.iter().position(|t| t.id == keep).unwrap());
        if self.queued.as_ref().is_some_and(|q| !self.session.tabs.iter().any(|t| t.id == q.tab.id)) { self.queued = None; }
        unsafe { self.rebuild_tabs(); }
        if old != self.session.active_tab().id { unsafe { self.navigate_current(); } }
        self.save();
    }

    unsafe fn search(&mut self) {
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_SEARCH) };
        let query = navigation::window_text(self.search_edit);
        if query.trim().is_empty() { let _ = unsafe { SetFocus(Some(self.search_edit)) }; return; }
        unsafe { self.run_search(query.trim()); }
    }

    /// Called after typing pauses: searches, or returns to the folder when cleared.
    pub(super) unsafe fn live_search(&mut self) {
        let query = navigation::window_text(self.search_edit);
        let query = query.trim();
        if query.is_empty() {
            if let Some(scope) = self.search_scope.take().filter(|_| self.in_search_results()) {
                self.search_result = None;
                unsafe { self.navigate_to(scope); }
            }
            return;
        }
        unsafe { self.run_search(query); }
    }

    pub(super) fn in_search_results(&self) -> bool { self.search_result.as_ref() == Some(self.current_location()) }

    unsafe fn run_search(&mut self, query: &str) {
        if self.pending.is_some() { unsafe { SetTimer(Some(self.hwnd), TIMER_SEARCH, 400, None); } return; }
        // Refining a search keeps the original folder as its scope.
        let scope = match (&self.search_scope, self.in_search_results()) {
            (Some(scope), true) => scope.clone(),
            _ => self.current_location().clone(),
        };
        let search_target=if self.session.ui.search_all_files {ShellLocation::ThisPc} else {scope.clone()};
        match zsdesk_windows::shell::search_location(&search_target, query) {
            Ok(location) => {
                self.search_scope = Some(scope);
                self.search_result = Some(location.clone());
                unsafe { self.navigate_to(location) };
            }
            Err(error) => self.show_message(format!("无法搜索：{error}")),
        }
    }
}

const SHORTCUTS: &str = "文件管理器\nCtrl+T 新建标签　Ctrl+W / Ctrl+F4 关闭标签　Ctrl+Shift+T 恢复标签\nCtrl+Tab / Ctrl+Shift+Tab 切换标签　Ctrl+1…9 第 N 个标签\nAlt+← / Backspace 后退　Alt+→ 前进　Alt+↑ 上一级　F5 刷新\nCtrl+L / Alt+D / F4 地址栏　Ctrl+F / Ctrl+E / F3 搜索（输入即搜）\nCtrl+Shift+N 新建文件夹　Ctrl+Shift+C 复制路径　Ctrl+D 收藏\nAlt+P 预览窗格　Alt+Shift+P 详细信息窗格\nF2 重命名　Del 删除　Shift+Del 永久删除　Alt+Enter 属性　Ctrl+A 全选\n标签：中键关闭，拖动排序；收藏：中键在新标签打开\n\n桌面格子\n鼠标悬停 展开折叠的格子　双击标题 折叠/展开　拖动标题 移动　拖动边缘 调整大小\nEnter 打开　F2 重命名　Del 删除　Ctrl+A / C / X / V\n方向键 选择　Backspace 返回上一级（文件夹格子）　F5 刷新\n双击桌面空白处 隐藏/显示全部格子";

fn desktop_host_running() -> bool {
    unsafe { FindWindowW(windows::core::w!("ZSDesk.Native.DesktopHost"), PCWSTR::null()) }.is_ok_and(|h| !h.0.is_null())
}

fn open_folder_options() {
    let _ = std::process::Command::new("rundll32.exe").args(["shell32.dll,Options_RunDLL", "0"]).spawn();
}

pub(super) fn set_clipboard_text(owner: HWND, text: &str) {
    use windows::Win32::{Foundation::HANDLE, System::{DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE}, Ole::CF_UNICODETEXT}};
    let data: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(Some(owner)).is_err() { return; }
        let _ = EmptyClipboard();
        if let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2) {
            let target = GlobalLock(memory) as *mut u16;
            if !target.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), target, data.len());
                let _ = GlobalUnlock(memory);
                let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(memory.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

/// Shows a ZSUI menu spec as a native popup. `anchor` (client rect) places it below a button.
pub(super) fn popup(owner: HWND, spec: &MenuSpec, anchor: Option<LayoutRect>) -> Option<String> {
    let mut ids: Vec<String> = Vec::new();
    let menu = build_menu(spec, &mut ids)?;
    let mut pt = POINT::default();
    let mut flags = TPM_RETURNCMD | TPM_RIGHTBUTTON;
    if let Some(r) = anchor {
        pt = POINT { x: r.x, y: r.bottom() };
        let _ = unsafe { ClientToScreen(owner, &mut pt) };
        flags |= TPM_LEFTALIGN | TPM_TOPALIGN;
    } else {
        let _ = unsafe { GetCursorPos(&mut pt) };
    }
    let selected = unsafe { TrackPopupMenu(menu, flags, pt.x, pt.y, None, owner, None) }.0;
    let _ = unsafe { DestroyMenu(menu) };
    selected.checked_sub(1).and_then(|i| ids.get(i as usize)).cloned()
}

fn build_menu(spec: &MenuSpec, ids: &mut Vec<String>) -> Option<HMENU> {
    let menu = unsafe { CreatePopupMenu() }.ok()?;
    for item in &spec.items {
        match item {
            MenuItemSpec::Command { label, command: UiCommand::Custom { id, .. }, checked, enabled, .. } => {
                ids.push(id.clone());
                let flags = MF_STRING | if *checked { MF_CHECKED } else { MF_UNCHECKED } | if *enabled { MF_ENABLED } else { MF_GRAYED };
                let _ = unsafe { AppendMenuW(menu, flags, ids.len(), PCWSTR(wide(label).as_ptr())) };
            }
            MenuItemSpec::Submenu { label, menu: inner, enabled, .. } => {
                if let Some(sub) = build_menu(inner, ids) {
                    let flags = MF_POPUP | if *enabled { MF_ENABLED } else { MF_GRAYED };
                    let _ = unsafe { AppendMenuW(menu, flags, sub.0 as usize, PCWSTR(wide(label).as_ptr())) };
                }
            }
            MenuItemSpec::Separator => { let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, None) }; }
            _ => {}
        }
    }
    Some(menu)
}
