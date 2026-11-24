use super::*;
use std::{ffi::OsString, os::windows::{ffi::OsStringExt, process::CommandExt}, path::Path, process::Command};
use windows::{core::PCWSTR, Win32::{Foundation::{HLOCAL, LocalFree}, UI::Shell::CommandLineToArgvW}};

#[derive(Clone)]
pub(super) struct NavigationRequest {
    pub tab:ExplorerTab,
    /// Started by clicking the navigation tree: the tree already shows the
    /// user's choice and is left alone when the folder opens.
    pub from_tree:bool,
    /// Started by opening a folder from the Quick access contents view.
    pub from_quick_access:bool,
}
impl NavigationRequest { pub(super) fn new(tab:ExplorerTab)->Self {Self{tab,from_tree:false,from_quick_access:false}} }

fn leaves_results(role:BrowserRole,pending_search:bool,target:&ShellLocation)->bool{
    !matches!(target,ShellLocation::Search{..})&&(role==BrowserRole::Results||pending_search)
}

impl WindowState {
    pub(super) fn navigation_tree_options(&self)->zsdesk_windows::shell::NamespaceTreeOptions {
        zsdesk_windows::shell::NamespaceTreeOptions{show_recycle_bin:self.session.ui.navigation_show_recycle_bin,
            show_network:self.session.ui.navigation_show_network,expand_this_pc:self.session.ui.navigation_expand_this_pc}
    }
    pub(super) fn rebuild_navigation_tree(&mut self) {
        self.tree=None;
        if !self.session.ui.show_tree{self.relayout();self.position_children();return;}
        match unsafe{NamespaceTreeHost::create_with_options(self.hwnd,win_rect(self.layout.tree),self.current_location(),self.navigation_tree_options())} {
            Ok(tree)=>{self.tree=Some(tree);self.theme_children();self.relayout();self.position_children();},
            Err(error)=>self.show_message(zsdesk_core::tr_format!("无法更新导航窗格：{error}", error=error)),
        }
    }
    pub(super) fn navigation_auto_expand(&self)->bool {
        self.session.ui.navigation_auto_expand.unwrap_or_else(NamespaceTreeHost::system_auto_expand)
    }
    pub(super) unsafe fn open_requested(&mut self,request:crate::instance::OpenRequest){
        self.activate_existing_folder_pane(&request.location);
        if self.pending.is_some(){
            if self.open_requests.len()<32{self.open_requests.push_back(request);}
            return;
        }
        let index=self.session.tabs.iter().position(|t|shell_registration::same_location(t.current(),&request.location));
        if index==Some(self.session.active_tab){
            let names=if request.selections.is_empty(){request.selection.into_iter().collect()}else{request.selections};
            self.defer_selection(names,false);
            return;
        }
        self.launch_selection=request.selection;
        self.launch_selections=request.selections;
        if let Some(index)=index {unsafe {self.select_tab(index);}}
        else {unsafe {self.new_tab(request.location);}}
    }
    pub(super) unsafe fn capture_active(&mut self) {
        if self.pending.is_some() {return;}
        let Some(id)=self.shown_tab else {return;};
        let mut snapshot=self.browser.as_ref().and_then(|b|unsafe {b.capture_view()});
        if !self.background_pane{if let Some(view)=snapshot.as_mut(){self.columns.preserve_tab_view(view);}}
        if let Some(tab)=self.session.tabs.iter_mut().find(|t|t.id==id) {tab.view=snapshot;}
    }
    pub(super) unsafe fn request_navigation(&mut self,request:NavigationRequest) {
        self.deferred_selection=None;
        self.navigation_epoch=self.navigation_epoch.wrapping_add(1);
        zsdesk_windows::shell::file_transfer::clear_origin(self.view_hwnd);
        self.folder_sizes.cancel();
        if !self.background_pane {self.end_address_edit();}
        let leaving_results=leaves_results(self.browser_role,self.pending.as_ref().is_some_and(|p|matches!(p.tab.current(),ShellLocation::Search{..})),request.tab.current());
        if self.pending.is_some()&&!leaving_results {self.queued=Some(request);return;}
        if leaving_results {
            // ResultsFolder can retain NAVIGATEONCE and a pending FillFromObject
            // completion. Retire its event source instead of waiting for it.
            self.pending=None;self.queued=None;
            self.cancel_fast_search();
            let _=unsafe{KillTimer(Some(self.hwnd),TIMER_SEARCH)};
            if let Err(error)=self.reset_browser_host(){self.show_message(zsdesk_core::tr_format!("无法打开文件夹：{error}", error=error));return;}
        }
        if !self.session.tabs.iter().any(|t|t.id==request.tab.id) {return;}
        let target=request.tab.current().clone();
        if !self.background_pane{self.columns.release_middle();self.columns.preview_file=None;}
        self.pending=Some(request);
        self.publish_listary_folder();
        // Invalidate the old query before clearing/replacing its native view.
        self.cancel_fast_search();
        let global=matches!(&target,ShellLocation::Search{..});
        if global && self.browser_role==BrowserRole::Ordinary{
            if let Err(error)=self.reset_browser_host(){self.show_message(zsdesk_core::tr_format!("无法创建搜索结果视图：{error}", error=error));unsafe{self.navigation_failed();}return;}
        }
        self.browser_role=if global{BrowserRole::Results}else{BrowserRole::Ordinary};
        let result=self.browser.as_ref().map(|b|unsafe {b.navigate(&target)});
        if !matches!(result,Some(Ok(()))) {unsafe {self.navigation_failed();}return;}
        unsafe {self.update_address();}
    }
    pub(super) unsafe fn navigate_current(&mut self) {
        let request=NavigationRequest::new(self.session.active_tab().clone());
        unsafe {self.request_navigation(request);}
    }
    pub(super) unsafe fn navigate_to(&mut self,location:ShellLocation) {
        unsafe {self.navigate_active(location,false,false);}
    }
    /// Opens a folder clicked in the navigation tree in the active tab.
    pub(super) unsafe fn navigate_from_tree(&mut self,location:ShellLocation) {
        unsafe {self.navigate_active(location,true,false);}
    }
    /// Opens a folder invoked from the current folder view and retains whether
    /// that view was the Quick access namespace until navigation completes.
    pub(super) unsafe fn navigate_from_contents(&mut self,location:ShellLocation) {
        let from_quick_access=NamespaceTreeHost::is_quick_access_location(self.current_location());
        unsafe {self.navigate_active(location,false,from_quick_access);}
    }
    unsafe fn navigate_active(&mut self,location:ShellLocation,from_tree:bool,from_quick_access:bool) {
        unsafe {self.capture_active();}
        let mut tab=self.session.active_tab().clone();
        let replacing_pending_search=self.pending.as_ref().is_some_and(|p|matches!(p.tab.current(),ShellLocation::Search{..}));
        if tab.navigate(location)||replacing_pending_search {unsafe {self.request_navigation(NavigationRequest{tab,from_tree,from_quick_access});}}
    }
    pub(super) unsafe fn navigation_completed(&mut self,location:ShellLocation) {
        self.apply_content_font();
        self.navigation_epoch=self.navigation_epoch.wrapping_add(1);
        self.view_hwnd=self.browser.as_ref().and_then(|b|b.view_window()).unwrap_or_default();
        if let Some(identity)=self.identity_for_source(self.browser_source){wheel_input::install(self.view_hwnd,self.hwnd,identity);
            let _=zsdesk_windows::shell::file_transfer::bind_origin(self.view_hwnd,identity.source,identity.tab,identity.epoch);}
        let completed_request=self.pending.is_some();
        let completed_tab = self.pending.as_ref().map(|request| request.tab.id).or(self.shown_tab);
        let active_completion = completed_tab == Some(self.session.active_tab().id);
        let location=self.pending.as_ref().map(|p|p.tab.current()).filter(|l|matches!(l,ShellLocation::Search{..})).cloned().unwrap_or(location);
        let to_search=matches!(&location,ShellLocation::Search{..}) || self.pending.as_ref().is_some_and(|p|Some(p.tab.current())==self.search_result.as_ref());
        if to_search {
            if active_completion { self.search_result=Some(location.clone()); }
            if completed_request {
                if let ShellLocation::Search { scope, .. } = &location {
                    let tab = self.pending.as_ref().map_or(self.session.active_tab().id, |request| request.tab.id);
                    let leaving = self.queued.as_ref().is_some_and(|request| request.tab.id == tab && !matches!(request.tab.current(), ShellLocation::Search { .. }));
                    if !leaving {
                        let origin = self.session.tabs.iter().find(|candidate| candidate.id == tab)
                            .map(|candidate| candidate.current().clone()).filter(|current| !matches!(current, ShellLocation::Search { .. }))
                            .unwrap_or_else(|| scope.clone().map(ShellLocation::Path).unwrap_or(ShellLocation::ThisPc));
                        let controls = self.search_panel.tabs.entry(tab).or_default();
                        controls.scope = scope.clone();
                        controls.origin.get_or_insert(origin);
                        if active_completion { self.search_panel.open = true; }
                    }
                }
            }
            if active_completion {
                self.search_scope = self.search_controls().origin;
                if !self.background_pane && self.queued.is_none() && unsafe{GetFocus()}!=self.search_edit {
                    if let ShellLocation::Search{query,..}=&location {
                        self.search_controls_mut().draft = Some(query.clone());
                        let _=unsafe{SetWindowTextW(self.search_edit,PCWSTR(wide(query).as_ptr()))};
                    }
                }
            }
        }
        else if self.pending.is_some() && active_completion {
            // Leaving the results: forget the scope and clear the box (unless typing).
            self.search_scope=None; self.search_result=None;
            self.search_controls_mut().origin = None;
            let scope=if self.session.ui.search_all_files&&!location.as_path().is_some_and(crate::fast_search::network::is_remote){None}else{super::integrated_search::font_scope::folder_scope(&location)};
            self.search_controls_mut().scope=scope;
            if !self.background_pane && unsafe {GetFocus()}!=self.search_edit {
                self.search_controls_mut().draft = Some(String::new());
                let _=unsafe {SetWindowTextW(self.search_edit,PCWSTR::null())};
            }
        }
        let mut from_tree=false;
        let mut from_quick_access=false;
        if let Some(mut request)=self.pending.take() {
            from_tree=request.from_tree;
            from_quick_access=request.from_quick_access;
            request.tab.replace_current(location);
            if let Some(index)=self.session.tabs.iter().position(|t|t.id==request.tab.id) {
                request.tab.locked=self.session.tabs[index].locked;
                self.shown_tab=Some(request.tab.id);
                self.session.tabs[index]=request.tab;
                if self.queued.is_none() {
                    if let (Some(browser),Some(view))=(&self.browser,&self.session.tabs[index].view) {
                        unsafe {browser.restore_view(view);}
                    }
                    let names=if self.launch_selections.is_empty(){self.launch_selection.take().into_iter().collect()}
                        else{self.launch_selection=None;std::mem::take(&mut self.launch_selections)};
                    self.defer_selection(names,false);
                }
            } else {self.shown_tab=None;}
        } else if let Some(id)=self.shown_tab {
            if let Some(tab)=self.session.tabs.iter_mut().find(|t|t.id==id) {tab.navigate(location);}
        }
        if self.queued.is_none() && self.shown_tab!=Some(self.session.active_tab().id) {
            self.queued=Some(NavigationRequest::new(self.session.active_tab().clone()));
        }
        self.publish_listary_folder();
        self.save();
        if !self.background_pane && self.queued.is_none() { self.restore_folder_layout(); }
        unsafe {self.rebuild_tabs();}
        if self.queued.is_none() {
            let current=self.current_location().clone();
            // Tree clicks preserve their row; only folder opens from the Quick access
            // contents view synchronize the tree to a Quick access entry.
            if !self.background_pane && !matches!(&current,ShellLocation::Search{..}) {
                if let Some(tree)=self.tree.as_ref().filter(|_|self.session.ui.show_tree) {
                    if from_tree {
                        unsafe { tree.adopt_location(&current); }
                    } else if from_quick_access {
                        unsafe { tree.select_quick_access_location(&current); }
                    }
                }
            }
            self.theme_children();
            self.refresh_status();
            if completed_request{if let ShellLocation::Search{query,..}=&current{self.start_fast_search(query);}}
            self.relayout();self.position_children();
        }
        if let Some(request)=self.queued.take() {unsafe {self.request_navigation(request);}}
        else if let Some(request)=self.open_requests.pop_front(){unsafe {self.open_requested(request);}}
    }
    pub(super) unsafe fn navigation_failed(&mut self) {
        self.navigation_epoch=self.navigation_epoch.wrapping_add(1);
        self.cancel_fast_search();self.browser_role=BrowserRole::Ordinary;
        self.launch_selection=None;
        self.launch_selections.clear();
        let failed=self.pending.take().map(|p|p.tab.current().address()).unwrap_or_default();
        self.show_message(zsdesk_core::tr_format!("无法打开：{failed}", failed=failed));
        if self.queued.is_none() {
            if let Some(index)=self.session.tabs.iter().position(|t|Some(t.id)==self.shown_tab) {self.session.active_tab=index;
                if self.columns.active&&!self.background_pane{self.restore_folder_layout();}
            }
        }
        unsafe {self.rebuild_tabs();}
        if self.queued.is_none(){if let Some(identity)=self.identity_for_source(self.browser_source){let _=zsdesk_windows::shell::file_transfer::bind_origin(self.view_hwnd,identity.source,identity.tab,identity.epoch);}}
        if let Some(request)=self.queued.take() {unsafe {self.request_navigation(request);}}
        else if let Some(request)=self.open_requests.pop_front(){unsafe {self.open_requested(request);}}
    }
    pub(super) unsafe fn new_tab(&mut self,location:ShellLocation) {
        unsafe {self.capture_active();}
        let id=self.session.take_id();
        self.session.tabs.push(ExplorerTab::new(id,location));
        self.session.active_tab=self.session.tabs.len()-1;
        unsafe {self.rebuild_tabs();self.navigate_current();}
    }
    pub(super) unsafe fn select_tab(&mut self,index:usize) {
        if index>=self.session.tabs.len(){return;}
        if index==self.session.active_tab{
            if self.side_tab_first.take().is_some(){self.side_tab_wheel=0;self.relayout();self.invalidate_rect(self.layout.tab_panel);}
            return;
        }
        self.end_address_edit();
        unsafe {self.capture_active();}
        self.session.active_tab=index;
        unsafe {self.rebuild_tabs();self.navigate_current();}
    }
    pub(super) unsafe fn close_tab(&mut self,index:usize) {
        if index>=self.session.tabs.len() {return;}
        if self.session.tabs[index].locked{self.show_message(zsdesk_core::i18n::text("此标签页已锁定，请先在右键菜单中解锁").into());return;}
        if self.session.tabs.len()==1 && self.session.ui.close_window_on_last_tab {
            if self.close_last_pane_tab() {return;}
            post(self.hwnd,WM_CLOSE,0,0);return;
        }
        unsafe {self.capture_active();}
        let active_id=self.session.active_tab().id;
        let removed=self.session.tabs.remove(index);
        self.closed_tabs.push(removed.clone());
        if self.closed_tabs.len()>20 {self.closed_tabs.remove(0);}
        if self.session.tabs.is_empty() {
            let id=self.session.take_id();self.session.tabs.push(ExplorerTab::new(id,ShellLocation::ThisPc));
        }
        self.session.active_tab=self.session.tabs.iter().position(|t|t.id==active_id).unwrap_or(index.min(self.session.tabs.len()-1));
        if self.queued.as_ref().map(|r|r.tab.id)==Some(removed.id) {self.queued=None;}
        unsafe {self.rebuild_tabs();}
        if removed.id==active_id {unsafe {self.navigate_current();}}
        self.save();
    }
    pub(super) unsafe fn reopen_tab(&mut self) {
        if let Some(mut tab)=self.closed_tabs.pop() {
            unsafe {self.capture_active();}
            tab.id=self.session.take_id();self.session.tabs.push(tab);self.session.active_tab=self.session.tabs.len()-1;
            unsafe {self.rebuild_tabs();self.navigate_current();}
        }
    }
    pub(super) unsafe fn travel(&mut self,back:bool) {
        if self.pending.is_some(){return;}
        unsafe {self.capture_active();}
        let mut tab=self.session.active_tab().clone();
        if (if back {tab.go_back()} else {tab.go_forward()}).is_some() {
            tab.view=None;unsafe {self.request_navigation(NavigationRequest::new(tab));}
        }
    }
    pub(super) unsafe fn go_up(&mut self) {
        let target=match self.current_location() {
            ShellLocation::Path(path)=>path.parent().map(|p|ShellLocation::Path(p.into())).unwrap_or(ShellLocation::ThisPc),
            ShellLocation::ThisPc=>return,
            location@ShellLocation::Shell {..}=>unsafe{zsdesk_windows::shell::shell_item(location)}.ok()
                .and_then(|item|unsafe{item.GetParent()}.ok())
                .and_then(|parent|unsafe{zsdesk_windows::shell::location_from_shell_item(&parent)})
                .unwrap_or(ShellLocation::ThisPc),
            ShellLocation::Search {scope,..}=>scope.clone().map(ShellLocation::Path).unwrap_or(ShellLocation::ThisPc),
        };
        unsafe {self.navigate_to(target);}
    }
    /// Refreshes breadcrumb, window title, search hint and button states.
    pub(super) unsafe fn update_address(&mut self) {
        if self.background_pane { return; }
        self.publish_listary_folder();
        unsafe { self.sync_search_tab_edit(); }
        let target=self.pending.as_ref().filter(|p|p.tab.id==self.session.active_tab().id).map(|p|p.tab.current().clone()).unwrap_or_else(||self.current_location().clone());
        if self.crumbs_for.as_ref()!=Some(&target) {
            self.crumbs=if let ShellLocation::Search{query,scope}=&target{
                vec![Crumb{title:scope.as_ref().map(|p|zsdesk_core::model::explorer::path_title(p)).unwrap_or_else(||zsdesk_core::i18n::text("全部本地磁盘").into()),
                    location:scope.clone().map(ShellLocation::Path).unwrap_or(ShellLocation::ThisPc)},
                    Crumb{title:zsdesk_core::tr_format!("搜索：{query}", query=query),location:target.clone()}]
            }else{unsafe {zsdesk_windows::shell::breadcrumb(&target)}};
            if self.crumbs.is_empty() {self.crumbs.push(zsdesk_windows::shell::Crumb{title:target.title(),location:target.clone()});}
            self.crumbs_for=Some(target.clone());
            self.layout_crumbs();
            let cue=wide(&if self.session.ui.search_all_files {zsdesk_core::i18n::text("快速搜索全部本地磁盘").to_string()} else {zsdesk_core::tr_format!("搜索 {}", target.title())});
            unsafe {SendMessageW(self.search_edit,EM_SETCUEBANNER,Some(WPARAM(1)),Some(LPARAM(cue.as_ptr() as isize)));}
        }
        // Switching tabs restores that tab's query; live typing owns the EDIT
        // text until its pending navigation catches up.
        if self.pending.is_none() && self.queued.is_none() && self.shown_tab == Some(self.session.active_tab().id)
            && unsafe { GetFocus() } != self.search_edit {
            let query = match &target { ShellLocation::Search { query, .. } => query.as_str(), _ => "" };
            if window_text(self.search_edit) != query {
                let _ = unsafe { SetWindowTextW(self.search_edit, PCWSTR(wide(query).as_ptr())) };
            }
        }
        // Do not overwrite an address while the user is editing it.
        if self.address_editing && unsafe {GetFocus()}!=self.address_edit {
            self.end_address_edit();
            let _=unsafe {SetWindowTextW(self.address_edit,PCWSTR(wide(&target.address()).as_ptr()))};
        }
        let title=if let Some(picker)=&self.picker{format!("{} — {} - ZSDesk",picker.options.title,target.title())}else{format!("{} - ZSDesk",target.title())};
        let _=unsafe {SetWindowTextW(self.hwnd,PCWSTR(wide(&title).as_ptr()))};
        self.invalidate();
    }
    pub(super) unsafe fn focus_address(&mut self) {
        self.address_editing=true;
        if let Some(identity)=self.identity_for_source(self.browser_source) {address_input::begin(self.address_edit,self.hwnd,identity);}
        self.hover_crumb=None;
        let text=match self.current_location() {ShellLocation::ThisPc=>zsdesk_core::i18n::text("此电脑").to_string(),l=>l.address()};
        let _=unsafe {SetWindowTextW(self.address_edit,PCWSTR(wide(&text).as_ptr()))};
        self.position_children();
        let _=unsafe {SetFocus(Some(self.address_edit))};
        unsafe {SendMessageW(self.address_edit,EM_SETSEL,Some(WPARAM(0)),Some(LPARAM(-1)));}
        self.invalidate_rect(self.layout.nav_row);
    }
    pub(super) fn end_address_edit(&mut self) {
        if !self.address_editing {return;}
        self.address_editing=false;
        address_input::end(self.address_edit);
        let _=unsafe {ShowWindow(self.address_edit,SW_HIDE)};
        self.invalidate_rect(self.layout.nav_row);
    }
    pub(super) unsafe fn commit_address(&mut self) {
        let text=window_text(self.address_edit);
        unsafe{self.commit_address_text(&text);}
    }
    pub(super) unsafe fn commit_address_text(&mut self,text:&str) {
        if text.encode_utf16().count()>32767||text.contains('\0'){self.show_message(zsdesk_core::i18n::text("路径过长或包含无效字符").into());return;}
        if matches!(self.current_location(),ShellLocation::Shell{parsing_name,pidl:Some(_),..} if parsing_name.trim().is_empty())
            &&text.trim()==self.current_location().address(){
            // A provider label is display-only. Preserve its PIDL when the
            // user accepts an unchanged address instead of parsing the label.
            self.end_address_edit();self.focus_view();return;
        }
        if let Some(Ok(request))=super::terminal::parse(&text) {
            if self.launch_terminal(&request) { self.end_address_edit(); }
            return;
        }
        let lower=text.trim().to_ascii_lowercase();
        if !crate::edition::NETWORK && ["ftp://","sftp://","ssh://","smb://"].iter().any(|s|lower.starts_with(s)) {
            self.show_message(zsdesk_core::tr_format!("ZSDesk {} 不支持 FTP/SSH 地址，请使用网络版或全量版", crate::edition::name()));
            return;
        }
        if sftp_to_unc(text.trim()).is_some() && !sshfs_installed() {
            self.show_message(zsdesk_core::i18n::text("SFTP/SSH 访问需要先安装 SSHFS-Win（WinFsp），安装后可直接输入 sftp:// 地址").into());
            return;
        }
        let working_directory = match self.current_location() {
            ShellLocation::Path(path) => Some(path.as_path()),
            ShellLocation::Search { scope: Some(path), .. } => Some(path.as_path()),
            _ => None,
        };
        if working_directory.is_some_and(|path|path.to_string_lossy().starts_with(r"\\"))
            && address_is_command_prompt(&text) {
            self.show_message(zsdesk_core::i18n::text("CMD 不支持网络路径作为工作目录，请使用 PowerShell 或 pwsh").into());
            return;
        }
        if let Some(mut command) = address_command(&text, working_directory) {
            // ZSDesk is a GUI-subsystem process; explicitly give console shells
            // their own console so interactive shells such as Nushell can attach.
            command.creation_flags(0x0000_0010); // CREATE_NEW_CONSOLE
            if command.spawn().is_ok() {
                self.end_address_edit();
                if let Some(browser) = &self.browser { unsafe { browser.focus(); } }
                return;
            }
        }
        match parse_address(text) {
            Some(location)=>{
                let location=match resolve_relative_address(location,self.current_location()){
                    Ok(location)=>location,Err(message)=>{self.show_message(message);return;},
                };
                self.end_address_edit();
                unsafe {self.navigate_to(location);}
                if let Some(browser)=&self.browser {unsafe {browser.focus();}}
            },
            None=>{self.show_message(zsdesk_core::i18n::text("请输入文件夹路径").into());}
        }
    }
}
fn address_is_command_prompt(text:&str)->bool {
    address_command_args(text).and_then(|args|args.into_iter().next())
        .and_then(|exe|Path::new(&exe).file_name().map(|name|name.to_string_lossy().into_owned()))
        .is_some_and(|name|name.eq_ignore_ascii_case("cmd")||name.eq_ignore_ascii_case("cmd.exe"))
}
/// Parses and prepares any Windows executable command, not a list of known shells.
fn address_command(text: &str, working_directory: Option<&Path>) -> Option<Command> {
    let args = address_command_args(text)?;
    let executable = args.first()?;
    let name = executable.to_string_lossy();
    let has_path = name.contains('\\') || name.contains('/');
    if name.contains(':') && !has_path { return None; }
    if has_path && !matches!(Path::new(executable).extension()?.to_string_lossy().to_ascii_lowercase().as_str(), "exe" | "com") {
        return None;
    }
    let mut command = Command::new(executable);
    command.args(&args[1..]);
    if let Some(directory) = working_directory { command.current_dir(directory); }
    Some(command)
}

fn address_command_args(text: &str) -> Option<Vec<OsString>> {
    let text = text.trim();
    if text.is_empty() || text.contains('\0') { return None; }
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let mut count = 0;
    let arguments = unsafe { CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut count) };
    if arguments.is_null() { return None; }
    let result = if count > 0 {
        Some(unsafe { std::slice::from_raw_parts(arguments, count as usize) }.iter().map(|argument| {
            let mut length = 0;
            unsafe { while *argument.0.add(length) != 0 { length += 1; } }
            OsString::from_wide(unsafe { std::slice::from_raw_parts(argument.0, length) })
        }).collect())
    } else { None };
    unsafe { let _ = LocalFree(Some(HLOCAL(arguments.cast()))); }
    result
}
fn resolve_relative_address(location:ShellLocation,current:&ShellLocation)->Result<ShellLocation,String>{
    let ShellLocation::Path(path)=&location else{return Ok(location);};
    if path.is_absolute(){return Ok(location);}
    if matches!(path.components().next(),Some(std::path::Component::Prefix(_))){return Err(zsdesk_core::i18n::text("请输入包含盘符和反斜杠的完整路径").into());}
    let base=current.as_path().filter(|path|path.is_absolute()).ok_or_else(||zsdesk_core::i18n::text("当前位置不是文件系统目录，请输入完整路径").to_string())?;
    std::path::absolute(base.join(path)).map(ShellLocation::Path).map_err(|error|zsdesk_core::tr_format!("路径无效：{error}", error=error))
}
pub(super) fn parse_address(text:&str)->Option<ShellLocation> {
    parse_address_with(text,|name|std::env::var(name).ok())
}
/// `lookup` resolves `%NAME%` references; tests pass a fixed table.
fn parse_address_with(text:&str,lookup:impl Fn(&str)->Option<String>)->Option<ShellLocation> {
    let expanded=expand_environment(text.trim().trim_matches('"'),lookup);
    let text=expanded.trim().trim_matches('"');
    if text.is_empty(){return None;}
    if text=="此电脑" || text.eq_ignore_ascii_case("this pc") || text.eq_ignore_ascii_case("shell:MyComputerFolder") {return Some(ShellLocation::ThisPc);}
    if has_prefix(text,"shell:") || text.starts_with("::{") {return Some(ShellLocation::Shell{pidl:None, parsing_name:text.into(),title:"Shell".into()});}
    let lower=text.to_ascii_lowercase();
    // FTP sites open in the Windows Shell FTP folder.
    if lower.starts_with("ftp://") {
        let host=text[6..].split(['/','\\']).next().unwrap_or("FTP").rsplit('@').next().unwrap_or("FTP").to_string();
        return Some(ShellLocation::Shell{pidl:None, parsing_name:text.into(),title:host});
    }
    if let Some(unc)=smb_to_unc(text) {return Some(ShellLocation::Path(unc));}
    if let Some(unc)=sftp_to_unc(text) {return Some(ShellLocation::Path(unc));}
    // Shell resolves availability; there is no synchronous filesystem probe on the UI thread.
    Some(ShellLocation::Path(PathBuf::from(text)))
}

fn has_prefix(text:&str,prefix:&str)->bool {
    text.get(..prefix.len()).is_some_and(|head|head.eq_ignore_ascii_case(prefix))
}

/// Expands `%NAME%` like the Explorer address bar, so `%AppData%\Microsoft`
/// and `%TEMP%` open their folders. Undefined names and a lone `%` stay
/// literal: folder names may legitimately contain percent signs.
pub(super) fn expand_environment(text:&str,lookup:impl Fn(&str)->Option<String>)->std::borrow::Cow<'_,str> {
    if !text.contains('%') {return std::borrow::Cow::Borrowed(text);}
    let mut result=String::with_capacity(text.len());
    let mut rest=text;
    while let Some(start)=rest.find('%') {
        result.push_str(&rest[..start]);
        let after=&rest[start+1..];
        let Some(end)=after.find('%') else {result.push_str(&rest[start..]);return result.into();};
        let name=&after[..end];
        match (!name.is_empty() && !name.contains(['=','\0','\\','/'])).then(||lookup(name)).flatten() {
            Some(value)=>{result.push_str(&value);rest=&after[end+1..];}
            // Keep the unmatched `%NAME` and rescan from the closing `%`, which
            // may begin the next reference, as ExpandEnvironmentStrings does.
            None=>{result.push('%');result.push_str(name);rest=&after[end..];}
        }
    }
    result.push_str(rest);
    result.into()
}

/// `smb://host/share/dir` → `\\host\share\dir`.
pub(super) fn smb_to_unc(text:&str)->Option<PathBuf> {
    let rest=strip_scheme(text,&["smb://"])?;
    let rest=rest.trim_matches('/');
    (!rest.is_empty()).then(||PathBuf::from(format!(r"\\{}",rest.replace('/',"\\"))))
}

/// `sftp://user@host:port/path` (or `ssh://`) → SSHFS-Win path
/// `\\sshfs.r\user@host!port\path` (absolute path on the server).
pub(super) fn sftp_to_unc(text:&str)->Option<PathBuf> {
    let rest=strip_scheme(text,&["sftp://","ssh://"])?;
    let (authority,path)=rest.split_once('/').unwrap_or((rest,""));
    if authority.is_empty() {return None;}
    let (user_host,port)=match authority.rsplit_once(':') {
        Some((h,p)) if !p.is_empty() && p.chars().all(|c|c.is_ascii_digit()) => (h,Some(p)),
        _ => (authority,None),
    };
    let mut target=format!(r"\\sshfs.r\{user_host}");
    if let Some(port)=port.filter(|p|*p!="22") {target.push('!');target.push_str(port);}
    for part in path.split('/').filter(|p|!p.is_empty()) {target.push('\\');target.push_str(part);}
    Some(PathBuf::from(target))
}

fn strip_scheme<'a>(text:&'a str,schemes:&[&str])->Option<&'a str> {
    schemes.iter().find_map(|s| text.get(..s.len()).filter(|head|head.eq_ignore_ascii_case(s)).and_then(|_|text.get(s.len()..)))
}

/// SSHFS-Win (WinFsp) provides the `\\sshfs*` network provider used for SFTP.
pub(super) fn sshfs_installed()->bool {
    ["ProgramFiles","ProgramFiles(x86)"].iter().filter_map(|v|std::env::var_os(v))
        .any(|dir|std::path::Path::new(&dir).join(r"SSHFS-Win\bin\sshfs-win.exe").exists())
}
pub(super) fn window_text(hwnd:HWND)->String {
    let length=unsafe {GetWindowTextLengthW(hwnd)}.max(0) as usize;
    let mut buffer=vec![0;length+1];let copied=unsafe {GetWindowTextW(hwnd,&mut buffer)}.max(0) as usize;
    String::from_utf16_lossy(&buffer[..copied])
}
#[cfg(test)] mod tests {
    use super::*;
    #[test]fn relative_addresses_use_the_visible_folder_not_the_program_directory(){
        let current=ShellLocation::Path(r"D:\工程\资料".into());
        assert_eq!(resolve_relative_address(ShellLocation::Path("中文目录".into()),&current).unwrap(),ShellLocation::Path(r"D:\工程\资料\中文目录".into()));
        assert_eq!(resolve_relative_address(ShellLocation::Path(r"..\图纸".into()),&current).unwrap(),ShellLocation::Path(r"D:\工程\图纸".into()));
        assert_eq!(resolve_relative_address(ShellLocation::Path(r"\根目录".into()),&current).unwrap(),ShellLocation::Path(r"D:\根目录".into()));
        assert!(resolve_relative_address(ShellLocation::Path("相对目录".into()),&ShellLocation::ThisPc).is_err());
        assert!(resolve_relative_address(ShellLocation::Path("C:目录".into()),&current).is_err());
    }
    #[test] fn unicode_addresses_never_slice_through_a_character() {
        for text in [r"D:\中文目录",r"C:\施工日志",r"E:\资料\图纸",r"\\服务器\共享\工程","中文目录","😀文件夹",r"C:\é中\文件"] {
            assert_eq!(parse_address(text),Some(ShellLocation::Path(text.into())),"{text}");
            assert_eq!(sftp_to_unc(text),None);assert_eq!(smb_to_unc(text),None);
        }
        for prefix in ["", "C:", "C:\\", "\\\\"] {for character in ['中','é','😀','한'] {
            for count in 1..=10 {let text=format!("{prefix}{}",character.to_string().repeat(count));
                assert!(parse_address(&text).is_some());assert!(sftp_to_unc(&text).is_none());}
        }}
    }
    #[test] fn address_accepts_shell_unc_and_quoted_paths_without_io() {
        assert_eq!(parse_address("  此电脑 "),Some(ShellLocation::ThisPc));
        assert_eq!(parse_address("\"D:\\not-yet-mounted\\folder\""),Some(ShellLocation::Path(PathBuf::from("D:\\not-yet-mounted\\folder"))));
        assert!(matches!(parse_address("shell:RecycleBinFolder"),Some(ShellLocation::Shell{..})));
        assert_eq!(parse_address(" "),None);
        assert!(matches!(parse_address("Shell:Fonts"),Some(ShellLocation::Shell{parsing_name,..}) if parsing_name=="Shell:Fonts"));
    }
    #[test] fn address_expands_environment_variables_like_explorer() {
        let lookup=|name:&str|match name.to_ascii_lowercase().as_str() {
            "appdata"=>Some(r"C:\Users\me\AppData\Roaming".to_string()),
            "temp"=>Some(r"C:\Users\me\AppData\Local\Temp".to_string()),
            "programfiles(x86)"=>Some(r"C:\Program Files (x86)".to_string()),
            _=>None,
        };
        assert_eq!(parse_address_with("%appdata%",lookup),Some(ShellLocation::Path(PathBuf::from(r"C:\Users\me\AppData\Roaming"))));
        assert_eq!(parse_address_with(" \"%TEMP%\\zs\" ",lookup),Some(ShellLocation::Path(PathBuf::from(r"C:\Users\me\AppData\Local\Temp\zs"))));
        assert_eq!(parse_address_with("%ProgramFiles(x86)%\\Common Files",lookup),
            Some(ShellLocation::Path(PathBuf::from(r"C:\Program Files (x86)\Common Files"))));
        assert_eq!(expand_environment(r"D:\100%\%missing%\%temp%",lookup),r"D:\100%\%missing%\C:\Users\me\AppData\Local\Temp");
        assert_eq!(expand_environment("50%",lookup),"50%");
        assert_eq!(expand_environment("%%temp%",lookup),r"%C:\Users\me\AppData\Local\Temp");
        assert!(matches!(expand_environment(r"C:\plain",lookup),std::borrow::Cow::Borrowed(_)));
    }
    #[test] fn remote_addresses_map_to_shell_and_unc() {
        assert!(matches!(parse_address("ftp://user@ftp.example.com/pub"),Some(ShellLocation::Shell{title,..}) if title=="ftp.example.com"));
        assert_eq!(parse_address("smb://nas/共享/文档"),Some(ShellLocation::Path(PathBuf::from(r"\\nas\共享\文档"))));
        assert_eq!(sftp_to_unc("sftp://dev@10.0.0.5/var/log"),Some(PathBuf::from(r"\\sshfs.r\dev@10.0.0.5\var\log")));
        assert_eq!(sftp_to_unc("SSH://dev@host:2222"),Some(PathBuf::from(r"\\sshfs.r\dev@host!2222")));
        assert_eq!(sftp_to_unc("sftp://host:22/"),Some(PathBuf::from(r"\\sshfs.r\host")));
        assert_eq!(sftp_to_unc("sftp://"),None);
        assert_eq!(sftp_to_unc(r"C:\sftp"),None);
    }
    #[test]
    fn address_commands_accept_arbitrary_executables_and_windows_quoted_arguments() {
        let args = address_command_args(r#"nu --config "C:\Users\A B\config.toml" -c "print hello""#).unwrap();
        assert_eq!(args, ["nu", "--config", r"C:\Users\A B\config.toml", "-c", "print hello"].map(OsString::from));
        assert!(address_command("any-terminal-name", None).is_some());
        assert!(address_command(r#""C:\Program Files\Nu\nu.exe" --version"#, None).is_some());
    }

    #[test]
    fn command_prompt_is_identified_even_with_an_absolute_quoted_path() {
        assert!(address_is_command_prompt("cmd /C cd"));
        assert!(address_is_command_prompt(r#""C:\Windows\System32\cmd.exe" /C cd"#));
        assert!(!address_is_command_prompt("pwsh -Command Get-Location"));
        // Parameterized commands fall back to general executable handling.
        assert!(super::super::terminal::parse("cmd /C cd").unwrap().is_err());
        assert!(address_command("cmd /C cd", Some(Path::new(r"\\server\share"))).is_some());
    }

    #[test]
    fn address_command_starts_an_executable_in_the_current_folder() {
        let directory = std::env::current_dir().unwrap();
        let output = address_command("cmd.exe /C cd", Some(&directory)).unwrap().output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim().to_ascii_lowercase(), directory.to_string_lossy().to_ascii_lowercase());
    }
}

#[cfg(test)]mod results_exit_tests{
    use super::*;
    #[test]fn folder_navigation_retires_results_drop_and_navigation_restrictions(){
        for target in [ShellLocation::ThisPc,ShellLocation::Path(PathBuf::from(r"E:\资料"))]{
            assert!(leaves_results(BrowserRole::Results,false,&target));
            assert!(leaves_results(BrowserRole::Fresh,true,&target));
            assert!(!leaves_results(BrowserRole::Ordinary,false,&target));
        }
        let search=ShellLocation::Search{query:"文件".into(),scope:None};
        assert!(!leaves_results(BrowserRole::Results,true,&search));
    }
}
