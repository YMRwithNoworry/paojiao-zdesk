//! Two independent native folder views with shared chrome for the active pane.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_BROWSER_SOURCE: AtomicUsize = AtomicUsize::new(1);

/// Runtime state of the inactive pane. Persisted tabs live in secondary_pane;
/// bookmarks, settings, next_id and the window frame are deliberately shared.
pub(super) struct InactivePane {
    browser: Option<ExplorerBrowserHost>,
    browser_source: usize,
    folder_sizes: folder_sizes::FolderSizes,
    view_hwnd: HWND,
    navigation_epoch: u64,
    pending: Option<NavigationRequest>,
    queued: Option<NavigationRequest>,
    shown_tab: Option<u64>,
    closed_tabs: Vec<ExplorerTab>,
    view_status: ViewStatus,
    view_mode: (i32, i32),
    search_panel: search_panel::SearchPanel,
    search_scope: Option<ShellLocation>,
    search_result: Option<ShellLocation>,
    browser_role: BrowserRole,
    fast_search: Option<crate::fast_search::FastSearch>,
    fast_generation: u64,
    fast_submitted: Option<integrated_search::SubmittedSearch>,
    fast_found: usize,
    search_progress: Option<String>,
    fast_events: std::collections::VecDeque<crate::fast_search::SearchEvent>,
    fast_reset_pending: bool,
    fast_clearing: Option<std::time::Instant>,
    fast_restore: Option<global_search::SelectionRestore>,
    wheel_remainder: i32,
    launch_selection: Option<std::ffi::OsString>,
    launch_selections: Vec<std::ffi::OsString>,
    deferred_selection: Option<deferred_selection::Pending>,
    open_requests: std::collections::VecDeque<crate::instance::OpenRequest>,
}

impl Default for InactivePane {
    fn default() -> Self {
        Self { browser:None, browser_source:0, folder_sizes:Default::default(), view_hwnd:HWND::default(), navigation_epoch:0, pending:None,
            queued:None, shown_tab:None, closed_tabs:Vec::new(), view_status:ViewStatus::default(),
            view_mode:(0,0), search_panel:Default::default(), search_scope:None, search_result:None,
            browser_role:BrowserRole::Fresh, fast_search:None, fast_generation:0, fast_submitted:None,
            fast_found:0, search_progress:None, fast_events:Default::default(), fast_reset_pending:false,
            fast_clearing:None, fast_restore:None, wheel_remainder:0, launch_selection:None,
            launch_selections:Vec::new(), deferred_selection:None, open_requests:Default::default() }
    }
}

impl Drop for InactivePane {
    fn drop(&mut self) {
        self.folder_sizes.cancel();
        if let Some(engine)=&self.fast_search {engine.cancel();}
    }
}

pub(super) fn browser_message(message:u32)->bool {
    matches!(message,WM_SHELL_NAVIGATED|WM_SHELL_FOLDER_INVOKED|WM_SHELL_NAVIGATION_FAILED|WM_SHELL_SELECTION_CHANGED)
}

fn discard_message(message:u32,lp:LPARAM) {
    if lp.0==0 { return; }
    unsafe { match message {
        WM_SHELL_NAVIGATED|WM_SHELL_FOLDER_INVOKED=>drop(Box::from_raw(lp.0 as *mut ShellLocation)),
        WM_SHELL_SELECTION_CHANGED=>acknowledge_selection_changed(lp.0),
        _=>{},
    }}
}

impl WindowState {
    pub(super) fn shell_entries(&self)->Vec<crate::shell_window_registry::Entry>{
        let mut entries=Vec::with_capacity(2);
        if self.browser.is_some(){if let Some(tab)=self.session.tabs.get(self.session.active_tab){
            if let Some(entry)=shell_registration::ready_entry(self.browser_source,tab,self.navigation_epoch,self.shown_tab,
                self.pending.is_some()||self.queued.is_some()){entries.push(entry);}
        }}
        if self.session.ui.dual_pane && self.other_pane.browser.is_some(){
            if let Some(tab)=self.session.secondary_pane.tabs.get(self.session.secondary_pane.active_tab){
                if let Some(entry)=shell_registration::ready_entry(self.other_pane.browser_source,tab,self.other_pane.navigation_epoch,
                    self.other_pane.shown_tab,self.other_pane.pending.is_some()||self.other_pane.queued.is_some()){entries.push(entry);}
            }
        }
        entries.sort_by_key(|entry|entry.identity.source);entries
    }

    pub(super) fn activate_shell_source(&mut self,source:usize)->bool{
        if source==self.browser_source{return true;}
        if source!=self.other_pane.browser_source||self.other_pane.browser.is_none()||!self.session.ui.dual_pane{return false;}
        let previous_epoch=self.navigation_epoch;
        let requested_epoch=self.other_pane.navigation_epoch;
        pane_input::discard(self.hwnd);
        self.activate_pane(!self.active_pane_right);
        self.navigation_epoch=requested_epoch;
        self.other_pane.navigation_epoch=previous_epoch;
        self.browser_source==source
    }

    pub(super) fn activate_existing_folder_pane(&mut self,location:&ShellLocation){
        if self.session.tabs.iter().any(|tab|shell_registration::same_location(tab.current(),location)){return;}
        if self.session.ui.dual_pane&&self.other_pane.browser.is_some()
            &&self.session.secondary_pane.tabs.iter().any(|tab|shell_registration::same_location(tab.current(),location)){
            self.activate_pane(!self.active_pane_right);
        }
    }

    pub(super) fn create_browser(&mut self)->windows::core::Result<ExplorerBrowserHost> {
        self.folder_sizes.cancel();
        self.browser_source=NEXT_BROWSER_SOURCE.fetch_add(1,Ordering::Relaxed).max(1);
        self.view_hwnd=HWND::default();
        let panes=Panes{preview:false,details:self.session.ui.show_details};
        let browser=unsafe {
            if cfg!(test) { ExplorerBrowserHost::create_transient_with_source(self.hwnd,win_rect(self.layout.browser),self.modern,panes,self.browser_source) }
            else { ExplorerBrowserHost::create_with_source(self.hwnd,win_rect(self.layout.browser),self.modern,panes,self.browser_source) }
        }?;
        if !cfg!(test) { browser.set_view_state_name(&view_memory::property_bag_name(&self.config_path))?; }
        browser.set_context_menu_options(crate::context_preferences::native(&self.session.ui.context_menu));
        browser.set_content_background(self.dark,colorref(self.palette.content));
        Ok(browser)
    }

    fn swap_pane_runtime(&mut self) {
        let other=&mut self.other_pane;
        macro_rules! swap { ($($field:ident),+ $(,)?) => { $(std::mem::swap(&mut self.$field,&mut other.$field);)+ }; }
        swap!(browser,browser_source,folder_sizes,view_hwnd,navigation_epoch,pending,queued,shown_tab,closed_tabs,view_status,
            view_mode,search_panel,search_scope,search_result,browser_role,fast_search,fast_generation,
            fast_submitted,fast_found,search_progress,fast_events,fast_reset_pending,fast_clearing,
            fast_restore,wheel_remainder,launch_selection,launch_selections,deferred_selection,open_requests);
        std::mem::swap(&mut self.session.tabs,&mut self.session.secondary_pane.tabs);
        std::mem::swap(&mut self.session.active_tab,&mut self.session.secondary_pane.active_tab);
        self.active_pane_right=!self.active_pane_right;
        self.crumbs_for=None;
    }

    pub(super) fn canonical_session(&self)->ExplorerSession {
        let mut session=self.session.clone();
        if self.active_pane_right {
            std::mem::swap(&mut session.tabs,&mut session.secondary_pane.tabs);
            std::mem::swap(&mut session.active_tab,&mut session.secondary_pane.active_tab);
        }
        session
    }

    pub(super) fn source_for_view(&self,view:HWND)->Option<usize> {
        if view.0.is_null(){return None;}
        if self.view_hwnd==view && self.pending.is_none() && self.queued.is_none() {return Some(self.browser_source);}
        (self.session.ui.dual_pane && self.other_pane.view_hwnd==view && self.other_pane.pending.is_none()
            && self.other_pane.queued.is_none()).then_some(self.other_pane.browser_source)
    }

    pub(super) fn source_for_input(&self,target:HWND)->Option<usize> {
        let other=self.other_pane.view_hwnd;
        if !other.0.is_null() && (target==other || unsafe{IsChild(other,target)}.as_bool()) {
            return self.source_for_view(other);
        }
        if self.pending.is_none() && self.queued.is_none(){Some(self.browser_source)}else{None}
    }

    pub(super) fn other_source(&self)->usize {self.other_pane.browser_source}
    pub(super) fn other_folder_destination(&self)->Option<PathBuf>{
        if !self.session.ui.dual_pane||self.other_pane.browser.is_none()||self.other_pane.pending.is_some()||self.other_pane.queued.is_some(){return None;}
        let tab=self.session.secondary_pane.tabs.get(self.session.secondary_pane.active_tab)?;
        if self.other_pane.shown_tab!=Some(tab.id){return None;}
        tab.current().as_path().map(PathBuf::from)
    }

    pub(super) fn transfer_to_other_pane(&mut self,moving:bool){
        if self.pending.is_some()||self.queued.is_some(){return;}
        let Some(destination)=self.other_folder_destination()else{return;};
        let Some(browser)=self.browser.as_ref()else{return;};
        let result=(||->windows::core::Result<()>{
            let view=unsafe{browser.current_view()}?;
            let paths=zsdesk_windows::shell::file_transfer::selected_paths(&view)?;
            if paths.is_empty(){return Ok(());}
            if paths.iter().all(|path|path.parent().is_some_and(|parent|crate::explorer_forwarder::same_path(parent,&destination))){
                return Err(windows::core::Error::new(windows::core::HRESULT(0x80070057u32 as i32),"两栏显示同一目录，无需复制或移动"));
            }
            // Snapshot and submit one native batch. No clipboard roundtrip,
            // recursive Rust copy, or second enumeration of selected files.
            zsdesk_windows::shell::file_transfer::transfer_paths(self.hwnd,paths,destination,moving)
        })();
        self.shell_result(Some(result));
    }
    pub(super) fn other_identity(&self)->Option<pane_input::Identity> {
        self.session.secondary_pane.tabs.get(self.session.secondary_pane.active_tab).map(|tab|pane_input::Identity{
            source:self.other_pane.browser_source,tab:tab.id,epoch:self.other_pane.navigation_epoch})
    }

    pub(super) fn pane_open_folder(&mut self,source:usize,path:PathBuf) {
        self.pane_open_folders(source,vec![path]);
    }

    pub(super) fn pane_open_folders(&mut self,source:usize,paths:Vec<PathBuf>) {
        let other=source!=self.browser_source;
        if other && (source!=self.other_pane.browser_source || self.other_pane.browser.is_none()){return;}
        if other {self.background_pane=true;self.swap_pane_runtime();self.relayout();}
        for path in paths.into_iter().take(32){
            let target=if path.is_dir(){path}else{path.parent().map(PathBuf::from).unwrap_or(path)};
            unsafe{self.new_tab(ShellLocation::Path(target));}
        }
        if other {self.swap_pane_runtime();self.background_pane=false;self.relayout();self.position_children();unsafe{self.update_address();}}
    }

    pub(super) fn pane_navigate_folder(&mut self,source:usize,location:ShellLocation) {
        let other=source!=self.browser_source;
        if other && (source!=self.other_pane.browser_source || self.other_pane.browser.is_none()){return;}
        if other {self.background_pane=true;self.swap_pane_runtime();self.relayout();}
        unsafe{self.navigate_to(location);}
        if other {self.swap_pane_runtime();self.background_pane=false;self.relayout();self.position_children();unsafe{self.update_address();}}
    }

    pub(super) fn pane_action(&mut self,source:usize,action:usize) {
        let other=source!=self.browser_source;
        if other && (source!=self.other_pane.browser_source || self.other_pane.browser.is_none()){return;}
        if other {self.background_pane=true;self.swap_pane_runtime();self.relayout();}
        unsafe{self.action(action,0);}
        if other {
            if self.session.ui.dual_pane && self.other_pane.browser.is_some(){self.swap_pane_runtime();}
            self.background_pane=false;self.relayout();self.position_children();unsafe{self.update_address();}
        }
    }

    /// Closing the final tab of one pane retains the other pane as the single
    /// view; closing the window is reserved for its final visible tab.
    pub(super) fn close_last_pane_tab(&mut self)->bool {
        if !self.session.ui.dual_pane || self.other_pane.browser.is_none() || self.session.secondary_pane.tabs.is_empty(){return false;}
        unsafe{self.capture_active();}
        let closed=self.session.active_tab().clone();
        self.folder_sizes.cancel();self.cancel_fast_search();self.browser=None;
        self.swap_pane_runtime();self.active_pane_right=false;
        self.navigation_epoch=self.navigation_epoch.wrapping_add(1);
        self.other_pane=InactivePane::default();self.session.secondary_pane=Default::default();
        self.session.ui.dual_pane=false;self.closed_tabs.push(closed);
        if self.closed_tabs.len()>20{self.closed_tabs.remove(0);}
        self.search_panel.edit_tab=None;
        self.relayout();self.position_children();unsafe{self.update_address();}
        if let Some(tree)=&self.tree {
            if !matches!(self.current_location(),ShellLocation::Search{..}) {
                unsafe{tree.select_location_with_policy(self.current_location(),self.navigation_auto_expand());}
            }
        }
        self.focus_view();self.save();true
    }

    pub(super) fn ensure_dual_pane(&mut self) {
        if !self.session.ui.dual_pane {
            if self.active_pane_right { self.activate_pane(false); }
            if self.other_pane.browser.is_some() {
                if let Some(browser)=&self.other_pane.browser {
                    if let Some(tab)=self.session.secondary_pane.tabs.get_mut(self.session.secondary_pane.active_tab) {
                        if self.other_pane.pending.is_none() { tab.view=unsafe{browser.capture_view()}; }
                    }
                }
                if let Some(engine)=&self.other_pane.fast_search { engine.cancel(); }
                self.other_pane=InactivePane::default();
            }
            self.relayout();self.position_children();self.save();return;
        }
        if self.other_pane.browser.is_some() { return; }
        if self.session.secondary_pane.tabs.is_empty() {
            let location=match self.current_location() {
                ShellLocation::Search{scope,..}=>scope.clone().map(ShellLocation::Path).unwrap_or(ShellLocation::ThisPc),
                current=>current.clone(),
            };
            let id=self.session.take_id();
            self.session.secondary_pane.tabs.push(ExplorerTab::new(id,location));
            self.session.secondary_pane.active_tab=0;
        }
        self.background_pane=true;
        self.swap_pane_runtime();self.relayout();
        match self.create_browser() {
            Ok(browser)=>{self.browser=Some(browser);unsafe{self.navigate_current();}},
            Err(error)=>{self.session.ui.dual_pane=false;self.status=format!("无法打开双栏：{error}");},
        }
        self.swap_pane_runtime();self.background_pane=false;
        self.relayout();self.position_children();unsafe{self.update_address();}self.save();
    }

    pub(super) fn toggle_dual_pane(&mut self) {
        self.session.ui.dual_pane=!self.session.ui.dual_pane;
        self.ensure_dual_pane();self.invalidate();
    }

    pub(super) fn activate_pane(&mut self,right:bool) {
        if right==self.active_pane_right || (!self.session.ui.dual_pane && right) || self.other_pane.browser.is_none() { return; }
        self.end_address_edit();
        let id=self.session.active_tab().id;
        self.search_panel.tabs.entry(id).or_default().draft=Some(navigation::window_text(self.search_edit));
        let _=unsafe{KillTimer(Some(self.hwnd),TIMER_SEARCH)};
        self.tab_drag=None;self.pressed=None;
        self.navigation_epoch=self.navigation_epoch.wrapping_add(1);
        self.swap_pane_runtime();
        self.navigation_epoch=self.navigation_epoch.wrapping_add(1);
        // The shared edit belongs to the newly activated pane even if both
        // panes last displayed a query in an identically positioned tab.
        self.search_panel.edit_tab=None;
        self.relayout();self.position_children();
        unsafe{self.update_address();}
        if let Some(tree)=&self.tree {
            if !matches!(self.current_location(),ShellLocation::Search{..}) {
                unsafe{tree.select_location_with_policy(self.current_location(),self.navigation_auto_expand());}
            }
        }
        self.invalidate();
    }

    pub(super) fn route_browser_message(&mut self,msg:u32,wp:WPARAM,lp:LPARAM)->Option<LRESULT> {
        let other=wp.0!=0 && wp.0!=self.browser_source;
        if other && (wp.0!=self.other_pane.browser_source || self.other_pane.browser.is_none()) {
            discard_message(msg,lp);return Some(LRESULT(0));
        }
        self.routing_pane_message=true;
        if other { self.background_pane=true;self.swap_pane_runtime();self.relayout(); }
        let result=unsafe{self.handle(msg,wp,lp)};
        if other {
            self.swap_pane_runtime();self.background_pane=false;
            self.relayout();self.position_children();unsafe{self.update_address();}
        }
        self.routing_pane_message=false;
        result
    }

    pub(super) fn poll_pane_searches(&mut self) {
        self.poll_fast_search();
        if self.other_pane.browser.is_some() && (self.other_pane.fast_search.is_some() || !self.other_pane.fast_events.is_empty()) {
            self.background_pane=true;self.swap_pane_runtime();self.relayout();
            self.poll_fast_search();
            self.swap_pane_runtime();self.background_pane=false;
            self.relayout();self.position_children();unsafe{self.update_address();}
        }
    }

    pub(super) fn select_created_in_other_pane(&mut self,location:&ShellLocation,name:&std::ffi::OsStr){
        if !self.session.ui.dual_pane||self.other_pane.browser.is_none(){return;}
        if !self.session.secondary_pane.tabs.get(self.session.secondary_pane.active_tab)
            .is_some_and(|tab|shell_registration::same_location(tab.current(),location)){return;}
        self.swap_pane_runtime();self.defer_selection(vec![name.to_os_string()],false);self.swap_pane_runtime();
    }

    pub(super) fn poll_pane_selections(&mut self){
        self.poll_deferred_selection();
        if self.other_pane.browser.is_some()&&self.other_pane.deferred_selection.is_some(){
            self.background_pane=true;self.swap_pane_runtime();self.poll_deferred_selection();self.swap_pane_runtime();self.background_pane=false;
        }
        if self.deferred_selection.is_none()&&self.other_pane.deferred_selection.is_none(){let _=unsafe{KillTimer(Some(self.hwnd),deferred_selection::TIMER)};}
    }

    pub(super) fn pane_wheel(&mut self,source:usize,delta:i32) {
        if source!=self.browser_source {
            if source!=self.other_pane.browser_source || self.other_pane.browser.is_none(){return;}
            self.activate_pane(!self.active_pane_right);
        }
        self.apply_wheel_delta(delta);
    }

    pub(super) fn cancel_folder_sizes(&mut self) {
        self.folder_sizes.cancel();self.other_pane.folder_sizes.cancel();
    }

    pub(super) fn poll_folder_sizes(&mut self) {
        self.sync_search_preferences();
        let location=self.current_location().clone();
        let enabled=self.session.ui.everything_folder_sizes&&!self.session.ui.search_backend.is_builtin();
        let message=self.folder_sizes.poll(self.browser.as_ref(),&location,enabled,self.pending.is_some(),&self.config_path);
        let other_location=self.session.secondary_pane.tabs.get(self.session.secondary_pane.active_tab).map(|tab|tab.current().clone());
        let other_message=other_location.and_then(|location|self.other_pane.folder_sizes.poll(self.other_pane.browser.as_ref(),&location,
            enabled && self.session.ui.dual_pane,self.other_pane.pending.is_some(),&self.config_path));
        if let Some(message)=message.or(other_message) {self.show_message(message);}
    }

    pub(super) fn cancel_all_search_backends(&mut self){
        self.cancel_fast_search();self.fast_search=None;self.folder_sizes.cancel();
        for controls in self.search_panel.tabs.values_mut(){controls.busy=false;}
        if let Some(search)=&self.other_pane.fast_search{search.cancel();}
        self.other_pane.fast_search=None;self.other_pane.fast_generation=0;self.other_pane.fast_submitted=None;
        self.other_pane.fast_events.clear();self.other_pane.fast_restore=None;self.other_pane.folder_sizes.cancel();
        self.other_pane.fast_clearing=None;self.other_pane.fast_reset_pending=false;self.other_pane.search_progress=None;
        for controls in self.other_pane.search_panel.tabs.values_mut(){controls.busy=false;}
    }

    pub(super) fn layout_dual_pane(&mut self) {
        if !self.session.ui.dual_pane { return; }
        let area=self.layout.browser;
        let gap=self.s(5).min(area.width.max(0));
        let header=self.s(28).min(area.height.max(0));
        let available=(area.width-gap).max(0);
        let minimum=self.s(160).min(available/2);
        let left_width=((i64::from(available)*i64::from(self.session.ui.pane_ratio))/1000)as i32;
        let left_width=left_width.clamp(minimum,(available-minimum).max(minimum));
        let right_x=area.x+left_width+gap;
        let left=LayoutRect::new(area.x,area.y+header,left_width,(area.height-header).max(0));
        let right=LayoutRect::new(right_x,area.y+header,available-left_width,(area.height-header).max(0));
        self.layout.pane_headers=[LayoutRect::new(left.x,area.y,left.width,header),LayoutRect::new(right.x,area.y,right.width,header)];
        self.layout.pane_splitter=LayoutRect::new(left.right(),area.y,gap,area.height);
        self.layout.browser=if self.active_pane_right {right}else{left};
        self.layout.other_browser=if self.active_pane_right {left}else{right};
    }

    pub(super) fn position_other_pane(&self) {
        if let Some(browser)=&self.other_pane.browser { unsafe{browser.resize(win_rect(self.layout.other_browser));} }
    }

    pub(super) fn theme_other_pane(&self) {
        if let Some(browser)=&self.other_pane.browser {browser.set_content_background(self.dark,colorref(self.palette.content));}
    }

    pub(super) fn pane_title(&self,right:bool)->String {
        if right==self.active_pane_right { self.current_location().address() }
        else { self.session.secondary_pane.tabs.get(self.session.secondary_pane.active_tab).map(|t|t.current().address()).unwrap_or_default() }
    }

    #[cfg(test)]pub(super) fn pane_selection_paths(&self,right:bool)->Vec<String>{
        let browser=if right==self.active_pane_right{self.browser.as_ref()}else{self.other_pane.browser.as_ref()};
        browser.map_or_else(Vec::new,ExplorerBrowserHost::selection_paths)
    }

    pub(super) fn destroy_other_pane(&mut self) {
        if let Some(browser)=&self.other_pane.browser {
            if self.other_pane.pending.is_none() {
                if let Some(tab)=self.session.secondary_pane.tabs.get_mut(self.session.secondary_pane.active_tab) {tab.view=unsafe{browser.capture_view()};}
            }
        }
        if let Some(engine)=&self.other_pane.fast_search {engine.cancel();}
        self.other_pane=InactivePane::default();
    }
}

pub(super) fn activate_for_message(slot:&Slot,message:&MSG) {
    let pointer=matches!(message.message,WM_LBUTTONDOWN|WM_RBUTTONDOWN|WM_MBUTTONDOWN|WM_XBUTTONDOWN|WM_MOUSEWHEEL);
    let keyboard=matches!(message.message,WM_KEYDOWN|WM_SYSKEYDOWN);
    if !pointer && !keyboard { return; }
    if let Ok(mut slot)=slot.try_borrow_mut() {
        let Some(state)=slot.as_mut().filter(|s|s.session.ui.dual_pane && s.other_pane.browser.is_some()) else{return;};
        if message.hwnd!=state.hwnd && !unsafe{IsChild(state.hwnd,message.hwnd)}.as_bool(){return;}
        let activate=if pointer {
            let mut point=message.pt;let _=unsafe{ScreenToClient(state.hwnd,&mut point)};
            state.layout.other_browser.contains(point.x,point.y)
        } else {
            let focus=unsafe{GetFocus()};let view=state.other_pane.view_hwnd;
            !view.0.is_null() && (focus==view || unsafe{IsChild(view,focus)}.as_bool())
        };
        if activate { state.activate_pane(!state.active_pane_right); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state()->WindowState {
        let mut session=ExplorerSession::default();
        session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(PathBuf::from(r"E:\left")))];
        session.secondary_pane.tabs=vec![ExplorerTab::new(2,ShellLocation::Path(PathBuf::from(r"E:\right")))];
        session.next_id=3;session.ui.dual_pane=true;
        WindowState::new(session,PathBuf::new(),None,false)
    }
    #[test]
    fn dual_pane_session_stays_canonical_when_right_is_active() {
        let mut state=state();let original=state.canonical_session();
        state.navigation_epoch=3;state.other_pane.navigation_epoch=8;
        state.swap_pane_runtime();
        assert!(state.active_pane_right);assert_eq!(state.current_location().address(),r"E:\right");
        assert_eq!(state.canonical_session(),original);assert_eq!(state.navigation_epoch,8);
        state.navigation_epoch+=1;state.swap_pane_runtime();
        assert_eq!(state.navigation_epoch,3);assert_eq!(state.other_pane.navigation_epoch,9);
        assert_eq!(state.canonical_session(),original);
    }
    #[test]
    fn dual_pane_geometry_is_bounded_on_narrow_and_high_dpi_windows() {
        let mut state=state();
        for dpi in [96,144,192] { for width in [0,200,640,1200] { for ratio in [200,500,800] {
            state.dpi=dpi;state.session.ui.pane_ratio=ratio;
            state.layout=ChromeLayout::default();state.layout.browser=LayoutRect::new(220,180,width,400);
            state.layout_dual_pane();let a=state.layout.browser;let b=state.layout.other_browser;
            assert!(a.width>=0 && b.width>=0);assert_eq!(a.width+state.layout.pane_splitter.width+b.width,width);
            assert!(a.right()<=b.x);assert_eq!(a.y,b.y);assert_eq!(a.height,b.height);
        }}}
    }
    #[test]
    fn obsolete_browser_notifications_do_not_change_tabs() {
        let mut state=state();state.browser_source=7;state.other_pane.browser_source=8;
        let original=state.canonical_session();
        let payload=Box::into_raw(Box::new(ShellLocation::ThisPc));
        state.route_browser_message(WM_SHELL_NAVIGATED,WPARAM(6),LPARAM(payload as isize));
        assert_eq!(state.canonical_session(),original);
    }
}

#[cfg(test)]
pub(super) fn native_exercise(slot:&Slot,output:&std::path::Path,pump:fn(u64))->Result<serde_json::Value> {
    use anyhow::ensure;
    let left=output.join("dual-left");let right=output.join("dual-right");let nested=right.join("nested");
    std::fs::create_dir_all(&left)?;std::fs::create_dir_all(&nested)?;
    std::fs::write(left.join("left-only.txt"),b"left")?;std::fs::write(right.join("right-only.txt"),b"right")?;
    let ready=||->Result<()> {
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(12);
        loop {pump(25);let done=slot.borrow().as_ref().is_some_and(|s|
            s.pending.is_none() && s.queued.is_none() && s.other_pane.pending.is_none() && s.other_pane.queued.is_none());
            if done{return Ok(());}
            ensure!(std::time::Instant::now()<deadline,"dual-pane navigation timed out");
        }
    };
    {
        let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();
        unsafe{state.navigate_to(ShellLocation::Path(left.clone()));}
    }
    ready()?;
    {let mut slot=slot.borrow_mut();slot.as_mut().unwrap().toggle_dual_pane();}
    ready()?;
    // Both panes initially browse the very same folder. Re-enter it from a
    // different folder, switch selection repeatedly and drain Shell callbacks.
    for _ in 0..4{
        {let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();state.activate_pane(true);
            unsafe{state.navigate_to(ShellLocation::Path(right.clone()));}}
        ready()?;
        {let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();
            unsafe{state.navigate_to(ShellLocation::Path(left.clone()));}}
        ready()?;
        for active_right in [false,true,false,true]{
            {let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();state.activate_pane(active_right);
                state.defer_selection(vec!["left-only.txt".into()],false);}
            pump(150);
            let slot=slot.borrow();let state=slot.as_ref().unwrap();
            ensure!(state.browser.as_ref().unwrap().selection_paths().iter().any(|path|path.ends_with("left-only.txt")),"same-directory pane selection lost");
        }
    }
    {slot.borrow_mut().as_mut().unwrap().activate_pane(false);}
    let created=left.join(format!("new-folder-selection-{}",std::process::id()));
    {slot.borrow_mut().as_mut().unwrap().select_created_folder(created.clone());}
    pump(100);std::fs::create_dir(&created)?;
    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(8);
    loop{pump(50);let selected=slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().selection_paths();
        if selected.iter().any(|path|PathBuf::from(path)==created){break;}
        ensure!(std::time::Instant::now()<deadline,"new folder was not selected after asynchronous enumeration");
    }
    let edit=unsafe{GetFocus()};
    ensure!(text_input_window(edit),"new folder did not enter inline rename");
    unsafe{SendMessageW(edit,WM_KEYDOWN,Some(WPARAM(VK_ESCAPE.0 as usize)),None);}pump(100);
    let revealed=left.join(format!("download-{}-2.exe",std::process::id()));
    {let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();
        unsafe{state.open_requested(crate::instance::OpenRequest{location:ShellLocation::Path(left.clone()),
            selection:Some(revealed.file_name().unwrap().to_os_string()),selections:Vec::new()});}}
    pump(100);std::fs::write(&revealed,b"selection fixture; not an executable")?;
    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(8);
    loop{pump(50);let selected=slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().selection_paths();
        if selected.iter().any(|path|PathBuf::from(path)==revealed){break;}
        ensure!(std::time::Instant::now()<deadline,"external reveal lost a late-arriving file selection");
    }
    {
        let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();
        ensure!(state.other_pane.browser.is_some(),"second Shell view missing");
        state.activate_pane(true);unsafe{state.navigate_to(ShellLocation::Path(right.clone()));}
        state.activate_pane(false);
    }
    ready()?;
    {
        let slot=slot.borrow();let state=slot.as_ref().unwrap();
        ensure!(!state.active_pane_right && state.current_location().as_path()==Some(left.as_path()),"background right navigation stole active pane");
        ensure!(state.session.secondary_pane.tabs[state.session.secondary_pane.active_tab].current().as_path()==Some(right.as_path()),"right folder did not finish independently");
        ensure!(state.browser.as_ref().unwrap().item_paths_limit(32).iter().any(|p|p.ends_with("left-only.txt")),"left view was replaced by right contents");
        ensure!(state.other_pane.browser.as_ref().unwrap().item_paths_limit(32).iter().any(|p|p.ends_with("right-only.txt")),"right view contents are wrong");
    }
    for (moving,kind) in [(false,"copy"),(true,"move")]{
        let name=format!("direct-pane-{kind}-{}.txt",std::process::id());let source=left.join(&name);let target=right.join(&name);
        ensure!(!source.exists()&&!target.exists(),"direct pane fixture already exists");
        std::fs::write(&source,b"native direct pane transfer")?;
        {let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();state.activate_pane(false);state.defer_selection(vec![name.clone().into()],false);}
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(8);
        loop{pump(25);if slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().selection_paths().iter().any(|path|PathBuf::from(path)==source){break;}
            ensure!(std::time::Instant::now()<deadline,"direct pane source did not materialize");}
        {slot.borrow_mut().as_mut().unwrap().transfer_to_other_pane(moving);}
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(8);
        loop{pump(25);if zsdesk_windows::shell::file_transfer::pending_operations()==0&&target.is_file()&&(!moving||!source.exists()){break;}
            ensure!(std::time::Instant::now()<deadline,"direct pane {kind} did not complete");}
        ensure!(std::fs::read(&target).with_context(||format!("read completed pane transfer {}",target.display()))?==b"native direct pane transfer","direct pane transfer changed contents");
        ensure!(source.exists()!=moving,"copy/move source semantics differ");
    }
    let view={let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();state.activate_pane(true);
        state.set_view_dimensions(windows::Win32::UI::Shell::FVM_ICON.0,48);state.view_hwnd};
    // Sent input bypasses the outer GetMessage loop, exercising the native
    // subclass path that the original direct wheel_zoom unit test missed.
    let mut bounds=RECT::default();unsafe{GetWindowRect(view,&mut bounds)?;}
    let packed=LPARAM((((bounds.left+10)as u16 as u32)|(((bounds.top+30)as u16 as u32)<<16))as isize);
    unsafe{SendMessageW(view,WM_MOUSEWHEEL,Some(WPARAM((120usize<<16)|8)),Some(packed));}
    pump(100);
    ensure!(slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().view_mode().is_some_and(|(_,size)|size==64),"sent Ctrl+wheel was not routed to right pane");
    {
        let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();
        unsafe{state.navigate_to(ShellLocation::Path(nested.clone()));}state.activate_pane(false);
    }
    ready()?;
    let before=slot.borrow().as_ref().unwrap().session.tabs.len();
    let right_tabs=slot.borrow().as_ref().unwrap().session.secondary_pane.tabs.len();
    {let slot=slot.borrow();let state=slot.as_ref().unwrap();
        let identity=state.other_identity().context("inactive pane identity")?;
        ensure!(pane_input::post_tab_paths(state.hwnd,identity,vec![right.clone(),nested.clone()]),"batch tab ticket rejected");}
    ready()?;
    {let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();
        ensure!(!state.active_pane_right&&state.session.secondary_pane.tabs.len()==right_tabs+2,"batched drop lost a path after the first tab changed its epoch");
        state.activate_pane(true);let active=state.session.active_tab;unsafe{state.close_group(active,false);}
        state.activate_pane(false);}
    ready()?;
    {let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();unsafe{state.new_tab(ShellLocation::Path(left.clone()));}}
    ready()?;
    {
        let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();
        let tab=state.layout.tabs.iter().find(|tab|tab.index==state.session.active_tab).context("new tab bounds")?.rect;
        state.double_click(POINT{x:tab.x+8,y:tab.y+tab.height/2});
        ensure!(state.session.tabs.len()==before,"double-click did not close one tab");
    }
    ready()?;
    {let mut slot=slot.borrow_mut();slot.as_mut().unwrap().toggle_dual_pane();}
    {
        let slot=slot.borrow();let state=slot.as_ref().unwrap();
        ensure!(state.other_pane.browser.is_none()&&!state.session.ui.dual_pane,"disabling retained second host");
        let saved=storage::load(&state.config_path)?;
        ensure!(saved.secondary_pane.tabs[saved.secondary_pane.active_tab].current().as_path()==Some(nested.as_path()),"disabled pane history was lost");
    }
    {let mut slot=slot.borrow_mut();slot.as_mut().unwrap().toggle_dual_pane();}
    ready()?;
    {
        let mut slot=slot.borrow_mut();let state=slot.as_mut().unwrap();
        state.activate_pane(true);ensure!(state.current_location().as_path()==Some(nested.as_path()),"reopened pane lost its folder");
        let old_source=state.browser_source;
        unsafe{state.navigate_to(ShellLocation::Path(right.clone()));state.close_tab(state.session.active_tab);}
        ensure!(!state.session.ui.dual_pane && !state.active_pane_right,"closing one pane's last tab did not retain the other pane");
        let late=Box::into_raw(Box::new(ShellLocation::Path(right.clone())));
        post(state.hwnd,WM_SHELL_NAVIGATED,old_source,late as isize);
    }
    ready()?;
    ensure!(slot.borrow().as_ref().unwrap().current_location().as_path()==Some(left.as_path()),"late closed-pane completion altered remaining pane");
    Ok(serde_json::json!({"independent_native_views":true,"background_navigation_does_not_steal_active_pane":true,
        "sent_ctrl_wheel":true,"double_click_closes_one_tab":true,"disable_releases_native_view":true,"right_folder_persisted":true,
        "last_pane_tab_preserves_other_pane":true,"closed_pane_late_navigation_rejected":true,
        "same_folder_two_panes_repeated_navigation_and_selection":true,"new_folder_delayed_selection_and_rename":true,
        "external_reveal_waits_for_file_enumeration":true,"batch_drop_preserves_all_paths_and_inactive_pane":true,
        "direct_other_pane_copy_and_move":true}))
}
