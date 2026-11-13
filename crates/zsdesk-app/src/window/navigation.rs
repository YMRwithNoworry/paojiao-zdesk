use super::*;
use std::{ffi::OsString, os::windows::{ffi::OsStringExt, process::CommandExt}, path::Path, process::Command};
use windows::{core::PCWSTR, Win32::{Foundation::{HLOCAL, LocalFree}, UI::Shell::CommandLineToArgvW}};

#[derive(Clone)]
pub(super) struct NavigationRequest {
    pub tab:ExplorerTab,
    /// Started by clicking the navigation tree: the tree already shows the
    /// user's choice and is left alone when the folder opens.
    pub from_tree:bool,
}
impl NavigationRequest { pub(super) fn new(tab:ExplorerTab)->Self {Self{tab,from_tree:false}} }

impl WindowState {
    pub(super) fn navigation_tree_options(&self)->zsdesk_windows::shell::NamespaceTreeOptions {
        zsdesk_windows::shell::NamespaceTreeOptions{show_recycle_bin:self.session.ui.navigation_show_recycle_bin,
            show_network:self.session.ui.navigation_show_network,expand_this_pc:self.session.ui.navigation_expand_this_pc}
    }
    pub(super) fn rebuild_navigation_tree(&mut self) {
        self.tree=None;
        match unsafe{NamespaceTreeHost::create_with_options(self.hwnd,win_rect(self.layout.tree),self.current_location(),self.navigation_tree_options())} {
            Ok(tree)=>{self.tree=Some(tree);self.theme_children();self.relayout();self.position_children();},
            Err(error)=>self.show_message(format!("无法更新导航窗格：{error}")),
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
        let snapshot=self.browser.as_ref().and_then(|b|unsafe {b.capture_view()});
        if let Some(tab)=self.session.tabs.iter_mut().find(|t|t.id==id) {tab.view=snapshot;}
    }
    pub(super) unsafe fn request_navigation(&mut self,request:NavigationRequest) {
        self.deferred_selection=None;
        self.navigation_epoch=self.navigation_epoch.wrapping_add(1);
        self.folder_sizes.cancel();
        if !self.background_pane {self.end_address_edit();}
        if self.pending.is_some() {self.queued=Some(request);return;}
        if !self.session.tabs.iter().any(|t|t.id==request.tab.id) {return;}
        let target=request.tab.current().clone();
        self.pending=Some(request);
        self.publish_listary_folder();
        // Invalidate the old query before clearing/replacing its native view.
        self.cancel_fast_search();
        let global=matches!(&target,ShellLocation::Search{..});
        if global && self.browser_role==BrowserRole::Ordinary{
            if let Err(error)=self.reset_browser_host(){self.show_message(format!("无法创建搜索结果视图：{error}"));unsafe{self.navigation_failed();}return;}
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
        unsafe {self.navigate_active(location,false);}
    }
    /// Opens a folder clicked in the navigation tree in the active tab.
    pub(super) unsafe fn navigate_from_tree(&mut self,location:ShellLocation) {
        unsafe {self.navigate_active(location,true);}
    }
    unsafe fn navigate_active(&mut self,location:ShellLocation,from_tree:bool) {
        unsafe {self.capture_active();}
        let mut tab=self.session.active_tab().clone();
        if tab.navigate(location) {unsafe {self.request_navigation(NavigationRequest{tab,from_tree});}}
    }
    pub(super) unsafe fn navigation_completed(&mut self,location:ShellLocation) {
        self.navigation_epoch=self.navigation_epoch.wrapping_add(1);
        self.view_hwnd=self.browser.as_ref().and_then(|b|b.view_window()).unwrap_or_default();
        if let Some(identity)=self.identity_for_source(self.browser_source){wheel_input::install(self.view_hwnd,self.hwnd,identity);}
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
            if !self.background_pane && unsafe {GetFocus()}!=self.search_edit {
                self.search_controls_mut().draft = Some(String::new());
                let _=unsafe {SetWindowTextW(self.search_edit,PCWSTR::null())};
            }
        }
        let mut from_tree=false;
        if let Some(mut request)=self.pending.take() {
            from_tree=request.from_tree;
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
        self.save();
        unsafe {self.rebuild_tabs();}
        if self.queued.is_none() {
            let current=self.current_location().clone();
            // Tree clicks already express an explicit tree choice. Other navigation
            // (file view, address bar, history, or tab restore) leaves the tree alone.
            if from_tree && !self.background_pane && !matches!(&current,ShellLocation::Search{..}) {
                if let Some(tree)=self.tree.as_ref().filter(|_|self.session.ui.show_tree) {
                    unsafe { tree.adopt_location(&current); }
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
        self.show_message(format!("无法打开：{failed}"));
        if self.queued.is_none() {
            if let Some(index)=self.session.tabs.iter().position(|t|Some(t.id)==self.shown_tab) {self.session.active_tab=index;}
        }
        unsafe {self.rebuild_tabs();}
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
        if index>=self.session.tabs.len() || index==self.session.active_tab {return;}
        self.end_address_edit();
        unsafe {self.capture_active();}
        self.session.active_tab=index;
        unsafe {self.rebuild_tabs();self.navigate_current();}
    }
    pub(super) unsafe fn close_tab(&mut self,index:usize) {
        if index>=self.session.tabs.len() {return;}
        if self.session.tabs[index].locked{self.show_message("此标签页已锁定，请先在右键菜单中解锁".into());return;}
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
                vec![Crumb{title:scope.as_ref().map(|p|zsdesk_core::model::explorer::path_title(p)).unwrap_or_else(||"全部本地磁盘".into()),
                    location:scope.clone().map(ShellLocation::Path).unwrap_or(ShellLocation::ThisPc)},
                    Crumb{title:format!("搜索：{query}"),location:target.clone()}]
            }else{unsafe {zsdesk_windows::shell::breadcrumb(&target)}};
            if self.crumbs.is_empty() {self.crumbs.push(zsdesk_windows::shell::Crumb{title:target.title(),location:target.clone()});}
            self.crumbs_for=Some(target.clone());
            self.layout_crumbs();
            let cue=wide(&if self.session.ui.search_all_files {"快速搜索全部本地磁盘".to_string()} else {format!("搜索 {}",target.title())});
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
        let _=unsafe {SetWindowTextW(self.hwnd,PCWSTR(wide(&format!("{} - ZSDesk",target.title())).as_ptr()))};
        self.invalidate();
    }
    pub(super) unsafe fn focus_address(&mut self) {
        self.address_editing=true;
        if let Some(identity)=self.identity_for_source(self.browser_source) {address_input::begin(self.address_edit,self.hwnd,identity);}
        self.hover_crumb=None;
        let text=match self.current_location() {ShellLocation::ThisPc=>"此电脑".to_string(),l=>l.address()};
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
        if matches!(self.current_location(),ShellLocation::Shell{parsing_name,pidl:Some(_),..} if parsing_name.trim().is_empty())
            &&text.trim()==self.current_location().address(){
            // A provider label is display-only. Preserve its PIDL when the
            // user accepts an unchanged address instead of parsing the label.
            self.end_address_edit();self.focus_view();return;
        }
        if let Some(request)=super::terminal::parse(&text) {
            match request {
                Ok(request)=>if self.launch_terminal(&request) { self.end_address_edit(); },
                Err(message)=>self.show_message(message.into()),
            }
            return;
        }
        let lower=text.trim().to_ascii_lowercase();
        if !crate::edition::NETWORK && ["ftp://","sftp://","ssh://","smb://"].iter().any(|s|lower.starts_with(s)) {
            self.show_message(format!("ZSDesk {} 不支持 FTP/SSH 地址，请使用网络版或全量版",crate::edition::name()));
            return;
        }
        if sftp_to_unc(text.trim()).is_some() && !sshfs_installed() {
            self.show_message("SFTP/SSH 访问需要先安装 SSHFS-Win（WinFsp），安装后可直接输入 sftp:// 地址".into());
            return;
        }
        let working_directory = match self.current_location() {
            ShellLocation::Path(path) => Some(path.as_path()),
            ShellLocation::Search { scope: Some(path), .. } => Some(path.as_path()),
            _ => None,
        };
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
        match parse_address(&text) {
            Some(location) => {
                self.end_address_edit();
                unsafe { self.navigate_to(location); }
                if let Some(browser) = &self.browser { unsafe { browser.focus(); } }
            }
            None => self.show_message("请输入文件夹路径或可执行命令".into()),
        }
    }
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
    schemes.iter().find_map(|s| (text.len()>=s.len() && text[..s.len()].eq_ignore_ascii_case(s)).then(||&text[s.len()..]))
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
    fn address_command_starts_an_executable_in_the_current_folder() {
        let directory = std::env::current_dir().unwrap();
        let output = address_command("cmd.exe /C cd", Some(&directory)).unwrap().output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim().to_ascii_lowercase(), directory.to_string_lossy().to_ascii_lowercase());
    }
}
