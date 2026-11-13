use super::*;
use super::search_panel::{SearchCategory,SearchControls,SEARCH_PAGE_SIZE};
use crate::fast_search::everything::SearchSort;
use zsdesk_windows::shell::commands::Column;

pub(super) struct SettingsJob{
    result:std::sync::mpsc::Receiver<anyhow::Result<()>>,
    thread:Option<std::thread::JoinHandle<()>>,
}
impl Drop for SettingsJob{fn drop(&mut self){if let Some(thread)=self.thread.take(){let _=thread.join();}}}

#[derive(Clone,Debug,PartialEq,Eq)]
pub(super) struct SubmittedSearch{
    tab:u64,
    expression:String,
    offset:u32,
    sort:SearchSort,
    executable:Option<PathBuf>,
    backend:crate::fast_search::SearchOptions,
}

/// Group user syntax so OR expressions cannot escape a chosen folder/category.
fn expression(raw:&str,controls:&SearchControls)->String{
    let mut terms=Vec::new();
    if let Some(scope)=&controls.scope{
        let path=scope.to_string_lossy().replace('/',"\\");
        terms.push(format!("\"{}\\\"",path.trim_end_matches('\\')));
    }
    let category=controls.category.query_prefix();
    if !category.is_empty(){terms.push(category.to_owned());}
    if !raw.trim().is_empty(){terms.push(format!("<{}>",raw.trim()));}
    terms.join(" ")
}

impl WindowState{
    pub(super) unsafe fn sync_search_tab_edit(&mut self) {
        if self.background_pane { return; }
        let id = self.session.active_tab().id;
        if self.search_panel.edit_tab == Some(id) { return; }
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_SEARCH) };
        if let Some(previous) = self.search_panel.edit_tab {
            if self.session.tabs.iter().any(|tab| tab.id == previous) {
                self.search_panel.tabs.entry(previous).or_default().draft = Some(navigation::window_text(self.search_edit));
            }
        }
        let location = self.current_location().clone();
        let fallback = match &location { ShellLocation::Search { query, .. } => query.clone(), _ => String::new() };
        let scope = match &location {
            ShellLocation::Search { scope, .. } => scope.clone(),
            location if !self.session.ui.search_all_files => location.as_path().map(PathBuf::from),
            _ => None,
        };
        let controls = self.search_panel.tabs.entry(id).or_insert_with(|| SearchControls { scope, ..Default::default() });
        let text = controls.draft.clone().unwrap_or(fallback);
        controls.draft = Some(text.clone());
        self.search_scope = controls.origin.clone();
        self.search_result = matches!(&location, ShellLocation::Search { .. }).then_some(location);
        self.search_panel.edit_tab = Some(id);
        let _ = unsafe { SetWindowTextW(self.search_edit, PCWSTR(wide(&text).as_ptr())) };
        // A focused EDIT sends EN_CHANGE for programmatic restoration as well.
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_SEARCH) };
    }

    pub(super) fn launch_search_settings(&mut self,service:bool){
        self.sync_search_preferences();
        if self.session.ui.search_backend.is_builtin(){self.show_message("当前使用自带搜索，可在搜索设置中调整索引选项".into());return;}
        self.poll_search_settings();
        if self.search_settings_job.is_some(){self.show_message("正在打开搜索设置…".into());return;}
        let profile=self.config_path.clone();let(sender,receiver)=std::sync::mpsc::sync_channel(1);
        match std::thread::Builder::new().name("zsdesk-search-settings".into()).spawn(move||{
            let result=(||{
                unsafe{OleInitialize(None)}?;
                struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
                if service{crate::fast_search::everything::enable_service(&profile)}else{crate::fast_search::everything::open_settings(&profile)}
            })();let _=sender.send(result);
        }){
            Ok(thread)=>self.search_settings_job=Some(SettingsJob{result:receiver,thread:Some(thread)}),
            Err(error)=>self.show_message(format!("无法打开搜索设置：{error}")),
        }
    }
    pub(super) fn poll_search_settings(&mut self){
        use std::sync::mpsc::TryRecvError;
        let result=self.search_settings_job.as_ref().map(|job|job.result.try_recv());
        match result{
            Some(Ok(result))=>{self.search_settings_job=None;if let Err(error)=result{self.show_message(format!("搜索设置未完成：{error:#}"));}},
            Some(Err(TryRecvError::Disconnected))=>{self.search_settings_job=None;self.show_message("搜索设置任务已结束，请重试".into());},_=>{},
        }
    }
    pub(super) unsafe fn open_integrated_search(&mut self){
        self.end_address_edit();
        unsafe { self.sync_search_tab_edit(); }
        if !self.search_panel.open{
            self.search_panel.open=true;
            let scope = match self.current_location() {
                ShellLocation::Search { scope, .. } => scope.clone(),
                location if !self.session.ui.search_all_files => location.as_path().map(PathBuf::from),
                _ => None,
            };
            if self.search_controls().origin.is_none() { self.search_controls_mut().scope = scope; }
            self.relayout();self.position_children();self.invalidate();
        }
        if unsafe{GetFocus()}!=self.search_edit{
            let _=unsafe{SetFocus(Some(self.search_edit))};
            unsafe{SendMessageW(self.search_edit,EM_SETSEL,Some(WPARAM(0)),Some(LPARAM(-1)));}
        }
    }

    pub(super) unsafe fn request_integrated_search(&mut self,reset_page:bool){
        unsafe { self.sync_search_tab_edit(); }
        let _=unsafe{KillTimer(Some(self.hwnd),TIMER_SEARCH)};
        self.search_panel.open=true;
        let raw=super::navigation::window_text(self.search_edit);
        self.search_controls_mut().draft = Some(raw.clone());
        if reset_page{self.search_controls_mut().offset=0;}
        let controls=self.search_controls();
        if raw.trim().is_empty()&&controls.category==SearchCategory::All{
            self.cancel_fast_search();
            let state=self.search_controls_mut();state.total=0;state.shown=0;state.busy=false;state.error=None;
            let original = self.search_controls_mut().origin.take().or_else(|| self.search_scope.take());
            self.search_scope = None;
            if let Some(original)=original{unsafe{self.leave_integrated_results(original);}}
            self.relayout();self.position_children();self.invalidate();return;
        }
        if !matches!(self.current_location(),ShellLocation::Search{..}){
            let origin = self.current_location().clone();
            self.search_controls_mut().origin = Some(origin.clone());
            self.search_scope=Some(origin);
        } else {
            self.search_scope = self.search_controls().origin;
        }
        let location=ShellLocation::Search{query:raw.trim().to_owned(),scope:controls.scope};
        if self.pending.is_some() {
            // Enter can race the live-search debounce while FillFromObject is
            // still completing. The latest controls are read at completion;
            // queuing the same location would clear/materialize it twice.
            let tab_id=self.session.active_tab().id;
            if self.pending.as_ref().is_some_and(|request|request.tab.id==tab_id&&request.tab.current()==&location){
                self.queued=None;
                return;
            }
            if self.queued.as_ref().is_some_and(|request|request.tab.id==tab_id&&request.tab.current()==&location){return;}
            // The committed tab can already equal the latest refinement while
            // an older query is materializing. It still must replace the queue.
            let mut tab = self.session.active_tab().clone();
            tab.navigate(location);
            unsafe { self.request_navigation(NavigationRequest::new(tab)); }
        } else if self.current_location()==&location{
            self.start_fast_search(raw.trim());
        }else{unsafe{self.navigate_to(location);}}
    }

    pub(super) unsafe fn close_integrated_search(&mut self){
        let origin = self.search_controls().origin.or_else(|| self.search_scope.clone());
        self.search_panel.open=false;
        self.cancel_fast_search();
        let _=unsafe{KillTimer(Some(self.hwnd),TIMER_SEARCH)};
        let _=unsafe{SetWindowTextW(self.search_edit,windows::core::w!(""))};
        self.search_panel.tabs.remove(&self.session.active_tab().id);
        self.search_scope = None;
        if matches!(self.current_location(),ShellLocation::Search{..})||self.pending.as_ref().is_some_and(|request|matches!(request.tab.current(),ShellLocation::Search{..})){
            let target=origin.unwrap_or(ShellLocation::ThisPc);
            unsafe{self.leave_integrated_results(target);}
        }
        self.relayout();self.position_children();
        if let Some(browser)=&self.browser{unsafe{browser.focus();}}
        self.invalidate();
    }

    pub(super) fn search_expression(&self,raw:&str)->String{expression(raw,&self.search_controls())}
    pub(super) fn submitted_search(&self,raw:&str)->SubmittedSearch{
        let controls=self.search_controls();
        SubmittedSearch{tab:self.session.active_tab().id,expression:expression(raw,&controls),offset:controls.offset,
            sort:controls.sort,executable:self.session.ui.everything_executable.clone(),backend:crate::fast_search::SearchOptions::from_ui(&self.session.ui)}
    }
    unsafe fn leave_integrated_results(&mut self,target:ShellLocation){
        if self.pending.is_some(){
            // The tab still points at the original folder while Search is
            // materializing, so navigate_to(original) would otherwise no-op.
            let mut tab=self.session.active_tab().clone();tab.navigate(target);
            unsafe{self.request_navigation(NavigationRequest::new(tab));}
        }else{unsafe{self.navigate_to(target);}}
    }
    pub(super) fn apply_search_sort(&self){
        if let Some(browser)=&self.browser{
            let (column,ascending)=match self.search_controls().sort{
                SearchSort::NameAsc=>(Column::Name,true),SearchSort::NameDesc=>(Column::Name,false),
                SearchSort::ModifiedDesc=>(Column::Modified,false),SearchSort::SizeDesc=>(Column::Size,false),
            };
            let _=browser.set_sort(column,ascending);
        }
    }
    pub(super) fn record_search_progress(&mut self,event:&crate::fast_search::SearchEvent){
        let shown=self.fast_found;let controls=self.search_controls_mut();
        controls.total=event.indexed;controls.shown=shown;controls.busy=!event.finished;controls.error=event.error.clone();
        // A deletion can leave the current page beyond the new end.
        if event.finished&&event.error.is_none()&&controls.offset>0&&u64::from(controls.offset)>=controls.total{
            controls.offset=(controls.total.saturating_sub(1)/u64::from(SEARCH_PAGE_SIZE)*u64::from(SEARCH_PAGE_SIZE))as u32;
            post(self.hwnd,WM_ACTION,ACTION_SEARCH_PAGE,0);
        }
    }
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn scope_keeps_sibling_paths_and_or_terms_out(){
        let controls=SearchControls{scope:Some(PathBuf::from(r"E:\中文 目录")),category:SearchCategory::Documents,..Default::default()};
        let query=expression("alpha | beta",&controls);
        assert!(query.starts_with("\"E:\\中文 目录\\\" ext:doc;"));
        assert!(query.ends_with("<alpha | beta>"));
    }
    #[test]fn categories_work_without_a_keyword(){
        let controls=SearchControls{category:SearchCategory::Folders,..Default::default()};
        assert_eq!(expression("",&controls),"folder:");
        assert_eq!(expression("会议",&SearchControls::default()),"<会议>");
    }
}
