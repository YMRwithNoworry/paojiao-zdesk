use super::*;
use std::{ffi::OsString, os::windows::{ffi::OsStringExt, process::CommandExt}, path::Path, process::Command};
use windows::{core::PCWSTR, Win32::{Foundation::{HLOCAL, LocalFree}, UI::Shell::CommandLineToArgvW}};

#[derive(Clone)]
pub(super) struct NavigationRequest { pub tab:ExplorerTab }

impl WindowState {
    pub(super) unsafe fn open_requested(&mut self,request:crate::instance::OpenRequest){
        if self.pending.is_some(){
            if self.open_requests.len()<32{self.open_requests.push_back(request);}
            return;
        }
        let index=self.session.tabs.iter().position(|t|t.current()==&request.location);
        if index==Some(self.session.active_tab){
            if !request.selections.is_empty(){if let Some(browser)=&self.browser{let _=browser.select_children(&request.selections);}return;}
            if let (Some(browser),Some(name))=(&self.browser,request.selection){let _=browser.select_child(&name);}
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
        if self.pending.is_some() {self.queued=Some(request);return;}
        if !self.session.tabs.iter().any(|t|t.id==request.tab.id) {return;}
        let target=request.tab.current().clone();
        self.pending=Some(request);
        // Invalidate the old query before clearing/replacing its native view.
        self.cancel_fast_search();
        let global=matches!(&target,ShellLocation::Search{scope:None,..});
        if global && self.browser_role==BrowserRole::Ordinary{
            if let Err(error)=self.reset_browser_host(){self.show_message(format!("无法创建搜索结果视图：{error}"));unsafe{self.navigation_failed();}return;}
        }
        self.browser_role=if global{BrowserRole::Results}else{BrowserRole::Ordinary};
        let result=self.browser.as_ref().map(|b|unsafe {b.navigate(&target)});
        if !matches!(result,Some(Ok(()))) {unsafe {self.navigation_failed();}return;}
        unsafe {self.update_address();}
    }
    pub(super) unsafe fn navigate_current(&mut self) {
        let request=NavigationRequest{tab:self.session.active_tab().clone()};
        unsafe {self.request_navigation(request);}
    }
    pub(super) unsafe fn navigate_to(&mut self,location:ShellLocation) {
        unsafe {self.capture_active();}
        let mut tab=self.session.active_tab().clone();
        if tab.navigate(location) {unsafe {self.request_navigation(NavigationRequest{tab});}}
    }
    pub(super) unsafe fn navigation_completed(&mut self,location:ShellLocation) {
        let completed_request=self.pending.is_some();
        let location=self.pending.as_ref().map(|p|p.tab.current()).filter(|l|matches!(l,ShellLocation::Search{..})).cloned().unwrap_or(location);
        let to_search=matches!(&location,ShellLocation::Search{..}) || self.pending.as_ref().is_some_and(|p|Some(p.tab.current())==self.search_result.as_ref());
        if to_search {
            self.search_result=Some(location.clone());
            if self.search_scope.is_none(){if let ShellLocation::Search{scope,..}=&location{self.search_scope=Some(scope.clone().map(ShellLocation::Path).unwrap_or(ShellLocation::ThisPc));}}
            if unsafe{GetFocus()}!=self.search_edit {if let ShellLocation::Search{query,..}=&location{let _=unsafe{SetWindowTextW(self.search_edit,PCWSTR(wide(query).as_ptr()))};}}
        }
        else if self.pending.is_some() {
            // Leaving the results: forget the scope and clear the box (unless typing).
            self.search_scope=None; self.search_result=None;
            if unsafe {GetFocus()}!=self.search_edit { let _=unsafe {SetWindowTextW(self.search_edit,PCWSTR::null())}; }
        }
        if let Some(mut request)=self.pending.take() {
            request.tab.replace_current(location);
            if let Some(index)=self.session.tabs.iter().position(|t|t.id==request.tab.id) {
                self.shown_tab=Some(request.tab.id);
                self.session.tabs[index]=request.tab;
                if self.queued.is_none() {
                    if let (Some(browser),Some(view))=(&self.browser,&self.session.tabs[index].view) {
                        unsafe {browser.restore_view(view);}
                    }
                    if let (Some(browser),Some(name))=(&self.browser,self.launch_selection.take()) {
                        if let Err(e)=browser.select_child(&name) {self.show_message(format!("无法选中项目：{e}"));}
                    }
                    if !self.launch_selections.is_empty(){if let Some(browser)=&self.browser{if let Err(e)=browser.select_children(&self.launch_selections){self.show_message(format!("无法选中项目：{e}"));}}self.launch_selections.clear();}
                }
            } else {self.shown_tab=None;}
        } else if let Some(id)=self.shown_tab {
            if let Some(tab)=self.session.tabs.iter_mut().find(|t|t.id==id) {tab.navigate(location);}
        }
        if self.queued.is_none() && self.shown_tab!=Some(self.session.active_tab().id) {
            self.queued=Some(NavigationRequest{tab:self.session.active_tab().clone()});
        }
        self.save();
        unsafe {self.rebuild_tabs();}
        if self.queued.is_none() {
            let current=self.current_location().clone();
            if !matches!(&current,ShellLocation::Search{..}){if let Some(tree)=self.tree.as_ref().filter(|_|self.session.ui.show_tree) {unsafe {tree.select_location(&current);}}}
            self.theme_children();
            self.refresh_status();
            if completed_request{if let ShellLocation::Search{query,scope:None}=&current{self.start_fast_search(query);}}
            self.relayout();self.position_children();
        }
        if let Some(request)=self.queued.take() {unsafe {self.request_navigation(request);}}
        else if let Some(request)=self.open_requests.pop_front(){unsafe {self.open_requested(request);}}
    }
    pub(super) unsafe fn navigation_failed(&mut self) {
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
        unsafe {self.capture_active();}
        self.session.active_tab=index;
        unsafe {self.rebuild_tabs();self.navigate_current();}
    }
    pub(super) unsafe fn close_tab(&mut self,index:usize) {
        if index>=self.session.tabs.len() {return;}
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
            tab.view=None;unsafe {self.request_navigation(NavigationRequest{tab});}
        }
    }
    pub(super) unsafe fn go_up(&mut self) {
        let target=match self.current_location() {
            ShellLocation::Path(path)=>path.parent().map(|p|ShellLocation::Path(p.into())).unwrap_or(ShellLocation::ThisPc),
            ShellLocation::ThisPc=>return,
            ShellLocation::Shell {..}=>ShellLocation::ThisPc,
            ShellLocation::Search {scope,..}=>scope.clone().map(ShellLocation::Path).unwrap_or(ShellLocation::ThisPc),
        };
        unsafe {self.navigate_to(target);}
    }
    /// Refreshes breadcrumb, window title, search hint and button states.
    pub(super) unsafe fn update_address(&mut self) {
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
        // Do not overwrite an address while the user is editing it.
        if self.address_editing && unsafe {GetFocus()}!=self.address_edit {
            let _=unsafe {SetWindowTextW(self.address_edit,PCWSTR(wide(&target.address()).as_ptr()))};
        }
        let _=unsafe {SetWindowTextW(self.hwnd,PCWSTR(wide(&format!("{} - ZSDesk",target.title())).as_ptr()))};
        self.invalidate();
    }
    pub(super) unsafe fn focus_address(&mut self) {
        self.address_editing=true;
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
        let _=unsafe {ShowWindow(self.address_edit,SW_HIDE)};
        self.invalidate_rect(self.layout.nav_row);
    }
    pub(super) unsafe fn commit_address(&mut self) {
        let text=window_text(self.address_edit);
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
    let text=text.trim().trim_matches('"');
    if text.is_empty(){return None;}
    if text=="此电脑" || text.eq_ignore_ascii_case("this pc") || text.eq_ignore_ascii_case("shell:MyComputerFolder") {return Some(ShellLocation::ThisPc);}
    if text.starts_with("shell:") || text.starts_with("::{") {return Some(ShellLocation::Shell{parsing_name:text.into(),title:"Shell".into()});}
    let lower=text.to_ascii_lowercase();
    // FTP sites open in the Windows Shell FTP folder.
    if lower.starts_with("ftp://") {
        let host=text[6..].split(['/','\\']).next().unwrap_or("FTP").rsplit('@').next().unwrap_or("FTP").to_string();
        return Some(ShellLocation::Shell{parsing_name:text.into(),title:host});
    }
    if let Some(unc)=smb_to_unc(text) {return Some(ShellLocation::Path(unc));}
    if let Some(unc)=sftp_to_unc(text) {return Some(ShellLocation::Path(unc));}
    // Shell resolves availability; there is no synchronous filesystem probe on the UI thread.
    Some(ShellLocation::Path(PathBuf::from(text)))
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
