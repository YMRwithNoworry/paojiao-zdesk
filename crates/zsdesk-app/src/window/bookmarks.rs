use super::*;
use zsdesk_core::model::explorer::{Bookmark, BookmarkDisplay, path_title};
use zsdesk_ui::menus::{entry, menu, submenu, MenuItemSpec};
use zsdesk_windows::shell::open_path;
use super::navigation::window_text;

/// Local "2026-11-10 14:05" for default category names.
fn chrono_like_now()->String{
    let time=unsafe{windows::Win32::System::SystemInformation::GetLocalTime()};
    format!("{:04}-{:02}-{:02} {:02}:{:02}",time.wYear,time.wMonth,time.wDay,time.wHour,time.wMinute)
}

/// Menus carry bookmark IDs so a reorder cannot redirect a command to another entry.
fn dropdown_items(bookmarks:&[Bookmark],root:Option<u64>)->Vec<MenuItemSpec>{
    use std::collections::HashMap;
    let mut children:HashMap<Option<u64>,Vec<&Bookmark>>=HashMap::new();
    for b in bookmarks{children.entry(b.parent_id).or_default().push(b);}
    fn level(children:&HashMap<Option<u64>,Vec<&Bookmark>>,parent:Option<u64>,depth:usize)->Vec<MenuItemSpec>{
        let mut items=Vec::new();
        for b in children.get(&parent).into_iter().flatten(){
            let label=b.display_name.replace('&',"&&");
            if !b.is_folder{items.push(entry(&label,&format!("link-{}",b.id),true,false));}
            else if depth==6{items.push(entry(&label,&format!("level-{}",b.id),true,false));}
            else{items.push(submenu(&label,level(children,Some(b.id),depth+1)));}
        }
        if items.is_empty(){items.push(entry(zsdesk_core::i18n::text("此分类为空"),"empty",false,false));}
        items.push(MenuItemSpec::Separator);
        if let Some(id)=parent{
            items.push(entry(zsdesk_core::i18n::text("收藏当前文件夹到此分类"),&format!("add-{id}"),true,false));
            items.push(entry(zsdesk_core::i18n::text("管理此分类"),&format!("edit-{id}"),true,false));
        }else{
            items.push(entry(zsdesk_core::i18n::text("收藏当前文件夹\tCtrl+D"),"add",true,false));
            items.push(entry(zsdesk_core::i18n::text("新建分类目录"),"new-folder",true,false));
            items.push(entry(zsdesk_core::i18n::text("将所有标签页保存为新分类"),"save-tabs",true,false));
        }
        if children.get(&parent).is_some_and(|c|!c.is_empty()){
            items.push(entry(zsdesk_core::i18n::text("管理收藏项"),&parent.map_or_else(||"manage".into(),|id|format!("manage-{id}")),true,false));
        }
        items
    }
    level(&children,root,0)
}

impl WindowState {
    pub(super) fn renamed_bookmark_index(&self)->Option<usize>{self.rename_id.and_then(|id|self.session.bookmarks.iter().position(|b|b.id==id))}
    pub(super) fn bookmark_index(&self,slot:usize)->Option<usize>{self.bookmark_order.get(slot).copied()}
    pub(super) fn bookmark_label(&self,index:usize)->String{
        let item=&self.session.bookmarks[index];
        if self.session.ui.bookmark_display!=BookmarkDisplay::Expanded{return item.display_name.clone();}
        let depth=self.session.bookmark_depth(index);
        format!("{}{}{}","› ".repeat(depth.min(4)),if item.is_folder{if item.collapsed{"▸ "}else{"▾ "}}else{""},item.display_name)
    }
    fn row_of_bookmarks(&self)->LayoutRect{
        if self.layout.bookmarks_row.is_empty(){self.layout.nav_row}else{self.layout.bookmarks_row}
    }
    pub(super) fn bookmark_rect(&self,index:usize)->LayoutRect{
        let mut anchor=index;
        if self.session.ui.bookmark_display==BookmarkDisplay::Categories{
            for _ in 0..self.session.bookmarks.len(){
                let Some(parent)=self.session.bookmarks.get(anchor).and_then(|b|b.parent_id)else{break;};
                let Some(i)=self.session.bookmarks.iter().position(|b|b.id==parent)else{break;};anchor=i;
            }
        }
        self.bookmark_order.iter().position(|&i|i==anchor).and_then(|slot|self.layout.bookmarks.get(slot).copied())
            .unwrap_or_else(||{if !self.layout.bookmark_overflow.is_empty()&&(self.layout.bookmarks_vertical||self.session.ui.bookmark_display!=BookmarkDisplay::Expanded){return self.layout.bookmark_overflow;}
                let row=self.layout.bookmarks_row;LayoutRect::new(row.x+self.s(8),row.y+self.s(2),self.s(220).min((row.width-self.s(16)).max(0)),self.s(28).min((row.height-self.s(4)).max(0)))})
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
            self.show_message(zsdesk_core::tr_format!("“{name}”已在收藏栏中", name=name));
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
        if bookmark.is_folder && self.session.ui.bookmark_display!=BookmarkDisplay::Expanded {
            if new_tab{unsafe{self.open_bookmark_folder_tabs(index);}}else{unsafe{self.bookmark_dropdown(Some(bookmark.id),self.bookmark_rect(index));}}return;
        }
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
        if targets.is_empty(){self.show_message(zsdesk_core::tr_format!("“{}”中没有可打开的文件夹", bookmark.display_name));return;}
        let first=self.session.tabs.len();
        for target in targets{unsafe{self.new_tab(target);}}
        if first<self.session.tabs.len(){unsafe{self.select_tab(first);}}
    }
    /// Keeps the open tabs for later: into an existing category or a new one.
    pub(super) unsafe fn save_tabs_to_category(&mut self,folder:Option<u64>){
        unsafe{self.capture_active();}
        let tabs:Vec<_>=self.session.tabs.iter().map(|tab|tab.current().clone()).collect();
        let name=zsdesk_core::tr_format!("标签页 {}", chrono_like_now());
        let id=self.session.save_tabs_to_bookmarks(folder,&name,None,&tabs);
        self.session.ui.show_bookmarks=true;self.save();unsafe{self.rebuild_bookmarks();}
        if folder.is_none(){if let Some(index)=self.session.bookmarks.iter().position(|b|b.id==id){unsafe{self.start_bookmark_rename(index);}}}
        else{self.show_message(zsdesk_core::tr_format!("已将 {} 个标签页保存到分类", tabs.len()));}
    }
    pub(super) unsafe fn bookmark_double_click(&mut self,slot:usize)->bool{
        let Some(index)=self.bookmark_index(slot)else{return false;};
        if !self.session.bookmarks[index].is_folder{return false;}
        if self.session.ui.bookmark_display!=BookmarkDisplay::Expanded{return true;}
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
        let mut destinations=vec![entry(zsdesk_core::i18n::text("收藏栏根目录"),"move-root",bookmark.parent_id.is_some(),false)];
        destinations.extend(self.session.bookmarks.iter().filter(|b|b.is_folder&&b.id!=bookmark.id).map(|b|
            entry(&b.display_name.replace('&',"&&"),&format!("move-{}",b.id),bookmark.parent_id!=Some(b.id)&&self.session.can_move_bookmark_to(bookmark.id,Some(b.id)),false)));
        let spec = menu(vec![
            entry(if bookmark.is_folder&&self.session.ui.bookmark_display==BookmarkDisplay::Expanded{if bookmark.collapsed{zsdesk_core::i18n::text("展开分类")}else{zsdesk_core::i18n::text("折叠分类")}}else{zsdesk_core::i18n::text("打开")}, "open", true, false),
            entry(if bookmark.is_folder{zsdesk_core::i18n::text("打开分类中的全部文件夹\t双击")}else{zsdesk_core::i18n::text("在新标签页中打开")}, "open-tab", true, false),
            entry(zsdesk_core::i18n::text("收藏当前文件夹到此分类"),"add-here",bookmark.is_folder,false),
            entry(zsdesk_core::i18n::text("将所有标签页保存到此分类"),"save-tabs-here",bookmark.is_folder,false),
            entry(zsdesk_core::i18n::text("新建分类目录"),"new-folder",true,false),
            MenuItemSpec::Separator,
            entry(zsdesk_core::i18n::text("重命名"), "rename", true, false),
            entry(zsdesk_core::i18n::text("向左移动"), "left", position > 0, false),
            entry(zsdesk_core::i18n::text("向右移动"), "right", position+1<siblings.len(), false),
            submenu(zsdesk_core::i18n::text("移动到分类"),destinations),
            MenuItemSpec::Separator,
            entry(if bookmark.is_folder{zsdesk_core::i18n::text("移除分类并保留其中收藏")}else{zsdesk_core::i18n::text("从收藏栏移除")}, "remove", true, false),
            entry(zsdesk_core::i18n::text("删除分类及其中的收藏…"), "remove-subtree", bookmark.is_folder, false),
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
                let text=wide(&zsdesk_core::tr_format!("删除分类“{}”及其中的收藏和子分类？\n\n实际文件和文件夹不会被删除。", bookmark.display_name));
                if unsafe{MessageBoxW(Some(self.hwnd),PCWSTR(text.as_ptr()),windows::core::PCWSTR(zsdesk_core::i18n::wide("删除收藏分类").as_ptr()),MB_OKCANCEL|MB_ICONQUESTION)}==IDOK {
                    self.session.remove_bookmark_subtree(bookmark.id);self.save();unsafe{self.rebuild_bookmarks();}
                }
            },
            Some(action)=>if let Some(parent)=action.strip_prefix("move-").and_then(|id|id.parse().ok()){self.move_bookmark_to(bookmark.id,Some(parent),None);},
            _ => {}
        }
    }
    unsafe fn bookmark_dropdown(&mut self,mut root:Option<u64>,anchor:LayoutRect){
        loop {
            let spec=menu(dropdown_items(&self.session.bookmarks,root));
            let Some(action)=input::popup(self.hwnd,&spec,Some(anchor))else{return;};
            let parse=|prefix:&str|action.strip_prefix(prefix).and_then(|n|n.parse::<u64>().ok());
            if let Some(id)=parse("level-"){root=Some(id);continue;}
            if action=="manage" || parse("manage-").is_some(){
                let parent=parse("manage-");
                let items=self.session.bookmarks.iter().filter(|b|b.parent_id==parent).map(|b|
                    entry(&b.display_name.replace('&',"&&"),&b.id.to_string(),true,false)).collect();
                if let Some(id)=input::popup(self.hwnd,&menu(items),Some(anchor)).and_then(|n|n.parse::<u64>().ok()){
                    if let Some(i)=self.session.bookmarks.iter().position(|b|b.id==id){unsafe{self.bookmark_menu_index(i);}}
                }
            }else if let Some(id)=parse("link-"){
                if let Some(i)=self.session.bookmarks.iter().position(|b|b.id==id&&!b.is_folder){unsafe{self.open_bookmark_index(i,false);}}
            }else if let Some(id)=parse("edit-"){
                if let Some(i)=self.session.bookmarks.iter().position(|b|b.id==id){unsafe{self.bookmark_menu_index(i);}}
            }else if let Some(id)=parse("add-"){
                if self.session.bookmarks.iter().any(|b|b.id==id&&b.is_folder){
                    let target=self.current_location().clone();self.add_bookmark_to(target.title(),target,Some(id));
                }
            }else{match action.as_str(){
                "add"=>self.add_current_bookmark(),"new-folder"=>unsafe{self.new_bookmark_folder(None)},
                "save-tabs"=>unsafe{self.save_tabs_to_category(None)},_=>{}
            }}
            return;
        }
    }
    pub(super) unsafe fn bookmarks_overflow(&mut self) {
        if self.session.ui.bookmark_display==BookmarkDisplay::Dropdown{unsafe{self.bookmark_dropdown(None,self.layout.bookmark_overflow);}return;}
        let hidden = self.layout.bookmarks.len();
        let mut items: Vec<MenuItemSpec> = self.bookmark_order.iter().copied().skip(hidden)
            .map(|i| submenu(&self.bookmark_label(i).replace('&',"&&"),vec![
                entry(zsdesk_core::i18n::text("打开 / 展开"),&format!("bm-{i}"),true,false),entry(zsdesk_core::i18n::text("管理收藏"),&format!("edit-{i}"),true,false)])).collect();
        items.push(entry(zsdesk_core::i18n::text("收藏当前文件夹\tCtrl+D"), "add", true, false));
        items.push(entry(zsdesk_core::i18n::text("新建分类目录"),"new-folder",true,false));
        items.push(entry(zsdesk_core::i18n::text("将所有标签页保存为新分类"),"save-tabs",true,false));
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
            entry(zsdesk_core::i18n::text("收藏当前文件夹\tCtrl+D"), "add", true, false),
            entry(zsdesk_core::i18n::text("新建分类目录"),"new-folder",true,false),
            entry(zsdesk_core::i18n::text("将所有标签页保存为新分类"),"save-tabs",true,false),
            MenuItemSpec::Separator,
            entry(zsdesk_core::i18n::text("显示收藏栏"), "toggle", true, self.session.ui.show_bookmarks),
        ]);
        match input::popup(self.hwnd, &spec, None).as_deref() {
            Some("add") => self.add_current_bookmark(),
            Some("new-folder")=>unsafe{self.new_bookmark_folder(None);},
            Some("save-tabs")=>unsafe{self.save_tabs_to_category(None);},
            Some("toggle") => { self.session.ui.show_bookmarks = !self.session.ui.show_bookmarks; self.save(); unsafe { self.rebuild_bookmarks(); } }
            _ => {}
        }
    }
    unsafe fn new_bookmark_folder(&mut self,parent:Option<u64>){
        unsafe{self.commit_bookmark_rename();}
        let id=self.session.take_id();let mut name=zsdesk_core::i18n::text("新建分类").to_string();let mut number=2;
        while self.session.bookmarks.iter().any(|b|b.parent_id==parent&&b.display_name==name){name=zsdesk_core::tr_format!("新建分类 {number}", number=number);number+=1;}
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
        let width=if self.layout.bookmarks_vertical{rect.width}else{rect.width.max(self.s(140))};
        let r = LayoutRect::new(rect.x + pad, rect.y + (rect.height - line) / 2, (width - pad * 2).max(1), line);
        unsafe {
            move_control(self.rename_hwnd, r);
            SendMessageW(self.rename_hwnd, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
            if !SetWindowSubclass(self.rename_hwnd, Some(edit_proc), ID_BOOKMARK_RENAME, self.hwnd.0 as usize).as_bool() { self.cancel_rename(); return; }
            SendMessageW(self.rename_hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            let _ = SetFocus(Some(self.rename_hwnd));
        }
        self.invalidate_rect(self.row_of_bookmarks());
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
        self.invalidate_rect(self.row_of_bookmarks());
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
                let (at,start,length)=if self.layout.bookmarks_vertical{(point.y,rect.y,rect.height)}else{(point.x,rect.x,rect.width)};
                if item.is_folder&&at>=start+length/4&&at<start+length-length/4{
                    self.move_bookmark_to(id,Some(item.id),None);
                }else{
                    let before=if at<start+length/2{Some(item.id)}else{
                        self.session.bookmarks.iter().skip(index+1).find(|b|b.parent_id==item.parent_id&&b.id!=id).map(|b|b.id)
                    };self.move_bookmark_to(id,item.parent_id,before);
                }
            },
            Hit::BookmarksBar|Hit::BookmarkOverflow=>self.move_bookmark_to(id,None,None),
            _=>{},
        }
    }
}

#[cfg(test)]
mod dropdown_tests{
    use super::*;
    #[test]
    fn dropdown_keeps_order_real_ids_and_collapsed_children(){
        let mut folder=Bookmark::folder(15,"A&B".into(),None);folder.collapsed=true;
        let bookmarks=vec![folder,Bookmark::folder(27,"内部".into(),Some(15)),Bookmark::link(90,"file".into(),ShellLocation::ThisPc,Some(27)),Bookmark::link(42,"同名".into(),ShellLocation::ThisPc,None)];
        let items=dropdown_items(&bookmarks,None);
        let MenuItemSpec::Submenu{label,menu,..}=&items[0]else{panic!("category")};assert_eq!(label,"A&&B");
        let MenuItemSpec::Submenu{menu:inner,..}=&menu.items[0]else{panic!("nested category")};
        assert!(matches!(&inner.items[0],MenuItemSpec::Command{command:zsdesk_ui::menus::UiCommand::Custom{id,..},..} if id=="link-90"));
        assert!(matches!(&items[1],MenuItemSpec::Command{command:zsdesk_ui::menus::UiCommand::Custom{id,..},..} if id=="link-42"));
        assert!(matches!(&dropdown_items(&[],None)[0],MenuItemSpec::Command{enabled:false,..}));
    }
}
