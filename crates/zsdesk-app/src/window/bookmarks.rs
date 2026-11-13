use super::*;
use zsdesk_core::model::explorer::{Bookmark, path_title};
use zsdesk_ui::menus::{entry, menu, submenu, MenuItemSpec};
use zsdesk_windows::shell::open_path;
use super::navigation::window_text;

/// Local "2026-11-10 14:05" for default category names.
fn chrono_like_now()->String{
    let time=unsafe{windows::Win32::System::SystemInformation::GetLocalTime()};
    format!("{:04}-{:02}-{:02} {:02}:{:02}",time.wYear,time.wMonth,time.wDay,time.wHour,time.wMinute)
}

impl WindowState {
    pub(super) fn renamed_bookmark_index(&self)->Option<usize>{self.rename_id.and_then(|id|self.session.bookmarks.iter().position(|b|b.id==id))}
    pub(super) fn bookmark_index(&self,slot:usize)->Option<usize>{self.bookmark_order.get(slot).copied()}
    pub(super) fn bookmark_label(&self,index:usize)->String{
        let item=&self.session.bookmarks[index];let depth=self.session.bookmark_depth(index);
        format!("{}{}{}","› ".repeat(depth.min(4)),if item.is_folder{if item.collapsed{"▸ "}else{"▾ "}}else{""},item.display_name)
    }
    pub(super) fn bookmark_rect(&self,index:usize)->LayoutRect{
        self.bookmark_order.iter().position(|&i|i==index).and_then(|slot|self.layout.bookmarks.get(slot).copied())
            .unwrap_or_else(||{let row=self.layout.bookmarks_row;LayoutRect::new(row.x+self.s(8),row.y+self.s(2),self.s(220).min(row.width-self.s(16)),row.height-self.s(4))})
    }
    pub(super) fn add_current_bookmark(&mut self) {
        let target = self.current_location().clone();
        self.add_bookmark(target.title(), target);
    }
    pub(super) fn add_bookmark_path(&mut self, path: PathBuf) { self.add_bookmark(path_title(&path), ShellLocation::Path(path)); }
    pub(super) fn add_bookmark(&mut self, name: String, target: ShellLocation) {
        self.add_bookmark_to(name,target,None);
    }
    fn add_bookmark_to(&mut self,name:String,target:ShellLocation,parent:Option<u64>){
        if self.session.bookmarks.iter().any(|b| !b.is_folder&&b.target == target&&b.parent_id==parent) {
            self.show_message(format!("“{name}”已在收藏栏中"));
            return;
        }
        let id = self.session.take_id();
        self.session.bookmarks.push(Bookmark::link(id,name,target,parent));
        self.session.ui.show_bookmarks = true;
        self.save();
        unsafe { self.rebuild_bookmarks(); }
    }
    pub(super) unsafe fn rebuild_bookmarks(&mut self) {
        self.measure_bookmarks();
        unsafe { self.resize(); }
    }
    pub(super) unsafe fn open_bookmark(&mut self, index: usize, new_tab: bool) {
        let Some(index)=self.bookmark_index(index)else{return;};
        unsafe{self.open_bookmark_index(index,new_tab);}
    }
    unsafe fn open_bookmark_index(&mut self,index:usize,new_tab:bool){
        let Some(bookmark) = self.session.bookmarks.get(index).cloned() else { return };
        if bookmark.is_folder{self.session.bookmarks[index].collapsed=!bookmark.collapsed;self.save();unsafe{self.rebuild_bookmarks();}return;}
        if let ShellLocation::Path(path) = &bookmark.target {
            if path.is_file() { unsafe { open_path(self.hwnd, path); } return; }
        }
        if new_tab { unsafe { self.new_tab(bookmark.target); } } else { unsafe { self.navigate_to(bookmark.target); } }
    }
    /// Opens every folder of a category (nested ones included) as new tabs;
    /// file links are skipped so a category never launches programs.
    pub(super) unsafe fn open_bookmark_folder_tabs(&mut self,index:usize){
        let Some(bookmark)=self.session.bookmarks.get(index).cloned().filter(|b|b.is_folder)else{return;};
        let targets:Vec<_>=self.session.bookmark_folder_links(bookmark.id).into_iter()
            .filter(|target|target.as_path().is_none_or(|path|!path.is_file())).collect();
        if targets.is_empty(){self.show_message(format!("“{}”中没有可打开的文件夹",bookmark.display_name));return;}
        let first=self.session.tabs.len();
        for target in targets{unsafe{self.new_tab(target);}}
        if first<self.session.tabs.len(){unsafe{self.select_tab(first);}}
    }
    /// Keeps the open tabs for later: into an existing category or a new one.
    pub(super) unsafe fn save_tabs_to_category(&mut self,folder:Option<u64>){
        unsafe{self.capture_active();}
        let tabs:Vec<_>=self.session.tabs.iter().map(|tab|tab.current().clone()).collect();
        let name=format!("标签页 {}",chrono_like_now());
        let id=self.session.save_tabs_to_bookmarks(folder,&name,None,&tabs);
        self.session.ui.show_bookmarks=true;self.save();unsafe{self.rebuild_bookmarks();}
        if folder.is_none(){if let Some(index)=self.session.bookmarks.iter().position(|b|b.id==id){unsafe{self.start_bookmark_rename(index);}}}
        else{self.show_message(format!("已将 {} 个标签页保存到分类",tabs.len()));}
    }
    pub(super) unsafe fn bookmark_double_click(&mut self,slot:usize)->bool{
        let Some(index)=self.bookmark_index(slot)else{return false;};
        if !self.session.bookmarks[index].is_folder{return false;}
        // The first click of the pair already toggled it; restore the state.
        self.session.bookmarks[index].collapsed=!self.session.bookmarks[index].collapsed;
        unsafe{self.open_bookmark_folder_tabs(index);self.rebuild_bookmarks();}true
    }
    pub(super) unsafe fn bookmark_menu(&mut self, index: usize) {
        let Some(index)=self.bookmark_index(index)else{return;};unsafe{self.bookmark_menu_index(index);}
    }
    unsafe fn bookmark_menu_index(&mut self,index:usize){
        let Some(bookmark)=self.session.bookmarks.get(index).cloned()else{return;};
        let siblings:Vec<_>=self.session.bookmarks.iter().filter(|b|b.parent_id==bookmark.parent_id).map(|b|b.id).collect();
        let position=siblings.iter().position(|id|*id==bookmark.id).unwrap_or(0);
        let mut destinations=vec![entry("收藏栏根目录","move-root",bookmark.parent_id.is_some(),false)];
        destinations.extend(self.session.bookmarks.iter().filter(|b|b.is_folder&&b.id!=bookmark.id).map(|b|
            entry(&b.display_name.replace('&',"&&"),&format!("move-{}",b.id),bookmark.parent_id!=Some(b.id)&&self.session.can_move_bookmark_to(bookmark.id,Some(b.id)),false)));
        let spec = menu(vec![
            entry(if bookmark.is_folder{if bookmark.collapsed{"展开分类"}else{"折叠分类"}}else{"打开"}, "open", true, false),
            entry(if bookmark.is_folder{"打开分类中的全部文件夹\t双击"}else{"在新标签页中打开"}, "open-tab", true, false),
            entry("收藏当前文件夹到此分类","add-here",bookmark.is_folder,false),
            entry("将所有标签页保存到此分类","save-tabs-here",bookmark.is_folder,false),
            entry("新建分类目录","new-folder",true,false),
            MenuItemSpec::Separator,
            entry("重命名", "rename", true, false),
            entry("向左移动", "left", position > 0, false),
            entry("向右移动", "right", position+1<siblings.len(), false),
            submenu("移动到分类",destinations),
            MenuItemSpec::Separator,
            entry(if bookmark.is_folder{"移除分类并保留其中收藏"}else{"从收藏栏移除"}, "remove", true, false),
            entry("删除分类及其中的收藏…", "remove-subtree", bookmark.is_folder, false),
        ]);
        match input::popup(self.hwnd, &spec, None).as_deref() {
            Some("open") => unsafe { self.open_bookmark_index(index, false) },
            Some("open-tab") if bookmark.is_folder => unsafe { self.open_bookmark_folder_tabs(index) },
            Some("open-tab") => unsafe { self.open_bookmark_index(index, true) },
            Some("save-tabs-here")=>unsafe{self.save_tabs_to_category(Some(bookmark.id));},
            Some("add-here")=>{let target=self.current_location().clone();self.add_bookmark_to(target.title(),target,Some(bookmark.id));},
            Some("new-folder")=>unsafe{self.new_bookmark_folder(if bookmark.is_folder{Some(bookmark.id)}else{bookmark.parent_id});},
            Some("rename") => unsafe { self.start_bookmark_rename(index) },
            Some("left") if position>0=>self.move_bookmark_to(bookmark.id,bookmark.parent_id,Some(siblings[position-1])),
            Some("right") if position+1<siblings.len()=>self.move_bookmark_to(bookmark.id,bookmark.parent_id,siblings.get(position+2).copied()),
            Some("move-root")=>self.move_bookmark_to(bookmark.id,None,None),
            Some("remove") => { self.session.remove_bookmark(bookmark.id); self.save(); unsafe { self.rebuild_bookmarks(); } },
            Some("remove-subtree") if bookmark.is_folder => {
                let text=wide(&format!("删除分类“{}”及其中的收藏和子分类？\n\n实际文件和文件夹不会被删除。",bookmark.display_name));
                if unsafe{MessageBoxW(Some(self.hwnd),PCWSTR(text.as_ptr()),windows::core::w!("删除收藏分类"),MB_OKCANCEL|MB_ICONQUESTION)}==IDOK {
                    self.session.remove_bookmark_subtree(bookmark.id);self.save();unsafe{self.rebuild_bookmarks();}
                }
            },
            Some(action)=>if let Some(parent)=action.strip_prefix("move-").and_then(|id|id.parse().ok()){self.move_bookmark_to(bookmark.id,Some(parent),None);},
            _ => {}
        }
    }
    pub(super) unsafe fn bookmarks_overflow(&mut self) {
        let hidden = self.layout.bookmarks.len();
        let mut items: Vec<MenuItemSpec> = self.bookmark_order.iter().copied().skip(hidden)
            .map(|i| submenu(&self.bookmark_label(i).replace('&',"&&"),vec![
                entry("打开 / 展开",&format!("bm-{i}"),true,false),entry("管理收藏",&format!("edit-{i}"),true,false)])).collect();
        items.push(entry("收藏当前文件夹\tCtrl+D", "add", true, false));
        items.push(entry("新建分类目录","new-folder",true,false));
        items.push(entry("将所有标签页保存为新分类","save-tabs",true,false));
        match input::popup(self.hwnd, &menu(items), Some(self.layout.bookmark_overflow)).as_deref() {
            Some("add") => self.add_current_bookmark(),
            Some("new-folder")=>unsafe{self.new_bookmark_folder(None);},
            Some("save-tabs")=>unsafe{self.save_tabs_to_category(None);},
            Some(id) => if let Some(i) = id.strip_prefix("bm-").and_then(|n| n.parse().ok()) { unsafe { self.open_bookmark_index(i, false); } }
                else if let Some(i)=id.strip_prefix("edit-").and_then(|n|n.parse().ok()){unsafe{self.bookmark_menu_index(i);}},
            None => {}
        }
    }
    pub(super) unsafe fn bookmarks_bar_menu(&mut self) {
        let spec = menu(vec![
            entry("收藏当前文件夹\tCtrl+D", "add", true, false),
            entry("新建分类目录","new-folder",true,false),
            entry("将所有标签页保存为新分类","save-tabs",true,false),
            MenuItemSpec::Separator,
            entry("显示收藏栏", "toggle", true, self.session.ui.show_bookmarks),
        ]);
        match input::popup(self.hwnd, &spec, None).as_deref() {
            Some("add") => self.add_current_bookmark(),
            Some("new-folder")=>unsafe{self.new_bookmark_folder(None);},
            Some("save-tabs")=>unsafe{self.save_tabs_to_category(None);},
            Some("toggle") => { self.session.ui.show_bookmarks = !self.session.ui.show_bookmarks; self.save(); unsafe { self.resize(); } }
            _ => {}
        }
    }
    unsafe fn new_bookmark_folder(&mut self,parent:Option<u64>){
        unsafe{self.commit_bookmark_rename();}
        let id=self.session.take_id();let mut name="新建分类".to_string();let mut number=2;
        while self.session.bookmarks.iter().any(|b|b.parent_id==parent&&b.display_name==name){name=format!("新建分类 {number}");number+=1;}
        self.session.bookmarks.push(Bookmark::folder(id,name,parent));self.session.ui.show_bookmarks=true;
        if let Some(parent)=parent{if let Some(folder)=self.session.bookmarks.iter_mut().find(|b|b.id==parent){folder.collapsed=false;}}
        self.save();unsafe{self.rebuild_bookmarks();self.start_bookmark_rename(self.session.bookmarks.len()-1);}
    }
    unsafe fn start_bookmark_rename(&mut self, index: usize) {
        unsafe { self.commit_bookmark_rename(); }
        let Some(bookmark) = self.session.bookmarks.get(index) else { return };
        let rect = self.bookmark_rect(index);
        self.rename_hwnd = unsafe { child(self.hwnd, "EDIT", &bookmark.display_name, ID_BOOKMARK_RENAME, ES_AUTOHSCROLL as u32, true) }.unwrap_or_default();
        self.rename_id = Some(bookmark.id);
        // Borderless EDIT inside the rounded text box painted by the chrome.
        let line = self.edit_line_height();
        let pad = self.s(8);
        let r = LayoutRect::new(rect.x + pad, rect.y + (rect.height - line) / 2, rect.width.max(self.s(140)) - pad * 2, line);
        unsafe {
            move_control(self.rename_hwnd, r);
            SendMessageW(self.rename_hwnd, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
            if !SetWindowSubclass(self.rename_hwnd, Some(edit_proc), ID_BOOKMARK_RENAME, self.hwnd.0 as usize).as_bool() { self.cancel_rename(); return; }
            SendMessageW(self.rename_hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            let _ = SetFocus(Some(self.rename_hwnd));
        }
        self.invalidate_rect(self.layout.bookmarks_row);
    }
    pub(super) unsafe fn commit_bookmark_rename(&mut self) {
        let Some(id) = self.rename_id.take() else { return };
        let text = window_text(self.rename_hwnd);
        let _ = unsafe { DestroyWindow(self.rename_hwnd) };
        self.rename_hwnd = HWND::default();
        if !text.trim().is_empty() {
            if let Some(b) = self.session.bookmarks.iter_mut().find(|b|b.id==id) { b.display_name = text.trim().into(); self.save(); }
        }
        unsafe { self.rebuild_bookmarks(); }
    }
    pub(super) unsafe fn cancel_rename(&mut self) {
        self.rename_id = None;
        if !self.rename_hwnd.0.is_null() { let _ = unsafe { DestroyWindow(self.rename_hwnd) }; self.rename_hwnd = HWND::default(); }
        self.invalidate_rect(self.layout.bookmarks_row);
    }
    pub(super) fn move_bookmark_to(&mut self,id:u64,parent:Option<u64>,before:Option<u64>){
        if self.session.move_bookmark_to(id,parent,before){
            if let Some(parent)=parent{if let Some(folder)=self.session.bookmarks.iter_mut().find(|b|b.id==parent){folder.collapsed=false;}}
            self.save();unsafe{self.rebuild_bookmarks();}
        }
    }
    pub(super) fn finish_bookmark_drag(&mut self,id:u64,point:POINT){
        match self.layout.hit(point.x,point.y){
            Hit::Bookmark(slot)=>{
                let Some(index)=self.bookmark_index(slot)else{return;};let item=self.session.bookmarks[index].clone();
                if item.id==id{return;}
                let rect=self.layout.bookmarks[slot];
                if item.is_folder&&point.x>=rect.x+rect.width/4&&point.x<rect.right()-rect.width/4{
                    self.move_bookmark_to(id,Some(item.id),None);
                }else{
                    let before=if point.x<rect.x+rect.width/2{Some(item.id)}else{
                        self.session.bookmarks.iter().skip(index+1).find(|b|b.parent_id==item.parent_id&&b.id!=id).map(|b|b.id)
                    };self.move_bookmark_to(id,item.parent_id,before);
                }
            },
            Hit::BookmarksBar|Hit::BookmarkOverflow=>self.move_bookmark_to(id,None,None),
            _=>{},
        }
    }
}
