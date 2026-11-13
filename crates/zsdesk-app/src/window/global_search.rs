use super::*;
use std::collections::HashSet;

thread_local! {
    static INPUT_WATCH: RefCell<Vec<(u64,std::rc::Weak<Cell<bool>>)>> = const { RefCell::new(Vec::new()) };
    static NEXT_INPUT_WATCH: Cell<u64> = const { Cell::new(1) };
}

struct InputWatch { id:u64, hook:HHOOK, cancelled:Rc<Cell<bool>> }
impl InputWatch {
    fn new()->Option<Self>{
        let hook=unsafe{SetWindowsHookExW(WH_GETMESSAGE,Some(search_input_hook),None,
            windows::Win32::System::Threading::GetCurrentThreadId())}.ok()?;
        let id=NEXT_INPUT_WATCH.with(|next|{let value=next.get();next.set(value.wrapping_add(1).max(1));value});
        let cancelled=Rc::new(Cell::new(false));
        let installed=INPUT_WATCH.with(|slot|slot.try_borrow_mut().map(|mut slot|{
            if slot.len()>=32{return false;}slot.push((id,Rc::downgrade(&cancelled)));true
        }).unwrap_or(false));
        if !installed {let _=unsafe{UnhookWindowsHookEx(hook)};return None;}
        Some(Self{id,hook,cancelled})
    }
}
impl Drop for InputWatch {
    fn drop(&mut self){
        INPUT_WATCH.with(|slot|{if let Ok(mut slot)=slot.try_borrow_mut(){slot.retain(|(id,_)|*id!=self.id);}});
        let _=unsafe{UnhookWindowsHookEx(self.hook)};
    }
}
fn selection_input(message:u32)->bool{
    matches!(message,WM_KEYDOWN|WM_SYSKEYDOWN|WM_CHAR|WM_SYSCHAR|WM_IME_CHAR|WM_IME_COMPOSITION|
        WM_LBUTTONDOWN|WM_RBUTTONDOWN|WM_MBUTTONDOWN|WM_XBUTTONDOWN|
        WM_NCLBUTTONDOWN|WM_NCRBUTTONDOWN|WM_NCMBUTTONDOWN|WM_NCXBUTTONDOWN|WM_POINTERDOWN|WM_TOUCH)
}
unsafe extern "system" fn search_input_hook(code:i32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if code>=0 && lp.0!=0 {
        let message=unsafe{&*(lp.0 as *const MSG)};
        if selection_input(message.message){
            INPUT_WATCH.with(|slot|{if let Ok(slot)=slot.try_borrow(){for (_,flag) in slot.iter(){
                if let Some(flag)=flag.upgrade(){
                    #[cfg(test)]if !flag.get(){eprintln!("search restore input observed: message=0x{:04X}",message.message);}
                    flag.set(true);
                }
            }}});
        }
    }
    // Thread-local, read-only observation also covers COM/menu message pumps.
    // Input contents are not retained and every message continues unchanged.
    unsafe{CallNextHookEx(None,code,wp,lp)}
}
fn selection_keys(paths:&[PathBuf])->HashSet<Vec<u16>>{
    paths.iter().map(|path|ExplorerBrowserHost::search_path_key(path)).collect()
}
struct SelectionPlan{
    generation:u64,original:Vec<PathBuf>,wanted:HashSet<Vec<u16>>,ready:Vec<PathBuf>,
    expected:HashSet<Vec<u16>>,revision:u64,retries:u8,finished:bool,
    before_clear:HashSet<Vec<u16>>,confirming:bool,
    wait_until:Option<std::time::Instant>,
}
impl SelectionPlan{
    fn new(generation:u64,paths:Vec<PathBuf>,revision:u64)->Self{
        let wanted=selection_keys(&paths);
        Self{generation,original:paths,wanted:wanted.clone(),ready:Vec::new(),expected:wanted,revision,retries:0,finished:false,
            before_clear:HashSet::new(),confirming:false,wait_until:None}
    }
    fn consistent(&self,generation:u64,cancelled:bool,selected:&[PathBuf])->bool{
        !cancelled && generation==self.generation && selection_keys(selected)==self.expected
    }
    fn queue(&mut self,paths:&[PathBuf]){
        let mut ready=selection_keys(&self.ready);
        for path in paths{
            let key=ExplorerBrowserHost::search_path_key(path);
            if self.wanted.contains(&key) && !self.expected.contains(&key) && ready.insert(key){self.ready.push(path.clone());self.retries=0;}
        }
    }
    fn cleared(&mut self,revision:u64){
        self.before_clear=self.expected.clone();self.ready.clear();self.expected.clear();self.revision=revision;self.retries=0;self.finished=false;self.confirming=false;self.wait_until=None;
    }
    fn wait_allowed(&mut self,now:std::time::Instant)->bool{
        let deadline=self.wait_until.get_or_insert(now+std::time::Duration::from_secs(2));now<*deadline
    }
    fn clearing_consistent(&self,items:&[PathBuf],selected:&[PathBuf])->bool{
        let live=selection_keys(items);
        let expected:HashSet<_>=self.before_clear.intersection(&live).cloned().collect();
        expected==selection_keys(selected)
    }
}
pub(super) struct SelectionRestore{plan:SelectionPlan,watch:InputWatch}

impl WindowState{
    fn discard_fast_restore(&mut self,_reason:&str){
        if let Some(_restore)=self.fast_restore.take(){
            #[cfg(test)]eprintln!("search restore discarded: reason={_reason} generation={} current_generation={} cancelled={} ready={:?} expected={} revision={} retries={} finished={} confirming={} original={:?}",
                _restore.plan.generation,self.fast_generation,_restore.watch.cancelled.get(),_restore.plan.ready,
                _restore.plan.expected.len(),_restore.plan.revision,_restore.plan.retries,_restore.plan.finished,
                _restore.plan.confirming,_restore.plan.original);
        }
    }
    pub(super) fn write_diagnostics(&self,request:&crate::instance::DiagnosticRequest)->anyhow::Result<()>{
        if request.output.exists(){
            let existing:serde_json::Value=serde_json::from_slice(&std::fs::read(&request.output)?)?;
            anyhow::ensure!(existing["schema"]=="ZSDesk.Diagnostics.v1","输出文件不是既有诊断报告");
        }
        let paths=self.browser.as_ref().map(|b|b.item_paths_limit(128)).unwrap_or_default();
        let selected=self.browser.as_ref().map(|b|b.selection_paths_limit(128)).unwrap_or_default();
        let status=self.browser.as_ref().map(|b|b.status()).unwrap_or_default();
        let value=serde_json::json!({"schema":"ZSDesk.Diagnostics.v1","request_id":request.request_id,
            "pid":std::process::id(),"hwnd":self.hwnd.0 as usize,"version":env!("CARGO_PKG_VERSION"),"location":self.current_location(),
            "native_view":self.browser.as_ref().and_then(|b|b.view_mode()),"fullscreen":self.fullscreen_restore.is_some(),
            "navigation_pending":self.pending.is_some(),"items":status.items,"selected":status.selected,
            "item_paths":paths,"selected_paths":selected,"fast_generation":self.fast_generation,"fast_found":self.fast_found,
            "search_maintenance_role":self.fast_search.as_ref().map(|engine|engine.maintenance_role()),
            "search_progress":self.search_progress,"message":self.status,"icon_cache_items":self.icons.len(),
            "search_materialization":self.browser.as_ref().map(|browser|browser.search_materialization_metrics()),
            "search_panel_open":self.search_panel.open,"address_editing":self.address_editing,
            "search_page":{"offset":self.search_controls().offset,"total":self.search_controls().total,"shown":self.search_controls().shown,"busy":self.search_controls().busy},
            "surface_bytes":self.surface.as_ref().map(|s|u64::from(s.width.max(0)as u32)*u64::from(s.height.max(0)as u32)*4).unwrap_or(0),
            "ui":self.session.ui,"menu":zsdesk_windows::shell::menu_diagnostics()});
        if let Some(parent)=request.output.parent(){std::fs::create_dir_all(parent)?;}
        let temp=request.output.with_extension("diagnostic.tmp");
        std::fs::write(&temp,serde_json::to_vec_pretty(&value)?)?;std::fs::rename(temp,&request.output)?;Ok(())
    }
    pub(super) fn cancel_fast_search(&mut self){
        if let Some(engine)=&self.fast_search{engine.cancel();}
        self.fast_generation=0;self.fast_submitted=None;self.fast_events.clear();self.search_progress=None;
        self.discard_fast_restore("query cancelled");self.fast_reset_pending=false;self.fast_clearing=None;
        let _=unsafe{KillTimer(Some(self.hwnd),TIMER_FAST_SEARCH)};
    }
    pub(super) fn reset_browser_host(&mut self)->windows::core::Result<()>{
        self.folder_sizes.cancel();
        self.cancel_fast_search();self.browser=None;self.browser_role=BrowserRole::Fresh;
        let browser=self.create_browser()?;
        browser.set_context_menu_options(crate::context_preferences::native(&self.session.ui.context_menu));
        self.browser=Some(browser);self.theme_children();Ok(())
    }
    pub(super) fn start_fast_search(&mut self,query:&str){
        self.sync_search_preferences();
        let submitted=self.submitted_search(query);
        if self.fast_generation!=0&&self.search_controls().busy&&self.fast_submitted.as_ref()==Some(&submitted){return;}
        let options=crate::fast_search::SearchOptions::from_ui(&self.session.ui);
        if self.fast_search.is_none(){
            match crate::fast_search::FastSearch::for_profile(&self.config_path,self.hwnd,WM_FAST_SEARCH,options){
                Ok(engine)=>self.fast_search=Some(engine),Err(error)=>{self.show_message(format!("无法启动搜索：{error}"));return;}
            }
        }
        let preferred=self.search_scope.as_ref().and_then(|s|s.as_path().map(PathBuf::from));
        self.fast_found=0;self.fast_events.clear();self.discard_fast_restore("query started");self.fast_reset_pending=true;self.fast_clearing=None;
        let controls=self.search_controls();let expression=self.search_expression(query);
        self.fast_generation=self.fast_search.as_ref().unwrap().start_page(&expression,preferred,controls.offset,super::search_panel::SEARCH_PAGE_SIZE,controls.sort);
        self.fast_submitted=Some(submitted);
        let controls=self.search_controls_mut();controls.busy=true;controls.error=None;controls.shown=0;
        self.apply_search_sort();
        self.search_progress=Some("正在搜索…".into());
        self.relayout();self.position_children();self.invalidate();
    }
    pub(super) fn search_backend_changed(&mut self){
        let query=match self.current_location(){ShellLocation::Search{query,..}=>Some(query.clone()),_=>None};
        self.cancel_all_search_backends();
        self.save();
        if let Err(error)=crate::quick_search::control::signal(&self.config_path,"reload"){
            self.show_message(format!("后台搜索设置重载失败：{error}"));
        }
        if let Some(query)=query{self.start_fast_search(&query);}
    }
    pub(super) fn rebuild_search_index(&mut self){
        self.sync_search_preferences();
        if !self.session.ui.search_backend.is_builtin(){self.launch_search_settings(false);return;}
        let query=match self.current_location(){ShellLocation::Search{query,..}=>Some(query.clone()),_=>None};
        self.cancel_all_search_backends();
        match crate::fast_search::FastSearch::for_profile(&self.config_path,self.hwnd,WM_FAST_SEARCH,crate::fast_search::SearchOptions::from_ui(&self.session.ui)){
            Ok(engine)=>{engine.rebuild();self.fast_search=Some(engine);self.show_message("正在后台重建自带索引，搜索时可查看进度".into());},
            Err(error)=>self.show_message(format!("无法重建索引：{error}")),
        }
        if let Some(query)=query{self.start_fast_search(&query);}
    }
    pub(super) fn sync_search_preferences(&mut self){
        if self.session.ui.search_preferences()!=self.search_saved_preferences{return;}
        if let Ok(saved)=crate::storage::load(&self.config_path){let preferences=saved.ui.search_preferences();
            if preferences!=self.search_saved_preferences{self.cancel_all_search_backends();self.session.ui.apply_search_preferences(&preferences);self.search_saved_preferences=preferences;}
        }
    }
    pub(super) fn persist_search_preferences(&mut self)->bool{
        match crate::storage::save_window_with_search(&self.config_path,&self.canonical_session(),self.secondary_window,&self.search_saved_preferences){
            Ok(preferences)=>{
                let mut previous=self.search_saved_preferences.clone();previous.folder_sizes=preferences.folder_sizes;
                if preferences!=previous{self.cancel_all_search_backends();crate::fast_search::settings_changed(&self.config_path);}
                else if preferences.folder_sizes!=self.search_saved_preferences.folder_sizes{self.cancel_folder_sizes();}
                self.session.ui.apply_search_preferences(&preferences);self.search_saved_preferences=preferences;true
            },
            Err(error)=>{self.session.ui.apply_search_preferences(&self.search_saved_preferences);self.show_message(format!("搜索设置未保存：{error}"));false},
        }
    }
    fn pending_selection_valid(&mut self)->bool{
        let mut defer=false;
        let mut reason="no restoration or browser";
        let valid=match (&mut self.fast_restore,&self.browser){
            (Some(restore),Some(browser))=>{
                if restore.watch.cancelled.get(){reason="input observed during validation";false}
                else if restore.plan.generation!=self.fast_generation{reason="generation changed during validation";false}
                else if !restore.plan.confirming && browser.search_selection_revision()==restore.plan.revision {true}
                else{match browser.search_selection_snapshot(){
                    Ok((paths,revision))=>{
                        let valid=restore.plan.consistent(self.fast_generation,restore.watch.cancelled.get(),&paths);
                        #[cfg(test)]eprintln!("search restore validate: valid={valid} cancelled={} expected={} actual={paths:?} old_revision={} new_revision={revision}",restore.watch.cancelled.get(),restore.plan.expected.len(),restore.plan.revision);
                        if valid{
                            restore.plan.revision=revision;restore.plan.confirming=false;
                            if restore.plan.ready.is_empty(){restore.plan.wait_until=None;}
                        }
                        else{reason="actual selection differs from expected selection";}valid
                    },
                    Err(error) if error.code().0 as u32==0x8000000A=>{
                        #[cfg(test)]eprintln!("search restore validation deferred: retry={} error={error:?}",restore.plan.retries);
                        restore.plan.retries=restore.plan.retries.saturating_add(1);
                        defer=restore.plan.wait_allowed(std::time::Instant::now());
                        if !defer{reason="selection materialization deadline expired";}false
                    },
                    Err(error)=>{
                        #[cfg(test)]eprintln!("search restore validation failed: error={error:?}");
                        let _=error;reason="selection snapshot failed";false
                    },
                }}
            },_=>false,
        };
        if !valid && !defer{self.discard_fast_restore(reason);}
        if defer{unsafe{SetTimer(Some(self.hwnd),TIMER_FAST_SEARCH,30,None);}}
        valid
    }
    fn reset_fast_results(&mut self)->windows::core::Result<()>{
        if !self.pending_selection_valid() && self.fast_restore.is_some(){return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000000Au32 as i32)));}
        let Some(browser)=&self.browser else{return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000000Au32 as i32)));};
        if self.fast_restore.is_none(){
            match browser.search_selection_snapshot(){
                Ok((paths,revision))=>{
                    #[cfg(test)]eprintln!("search restore capture: paths={paths:?} revision={revision}");
                    if !paths.is_empty(){if let Some(watch)=InputWatch::new(){
                        self.fast_restore=Some(SelectionRestore{plan:SelectionPlan::new(self.fast_generation,paths,revision),watch});
                    }else{#[cfg(test)]eprintln!("search restore capture skipped: input watch unavailable");}}
                },
                Err(error)=>{#[cfg(test)]eprintln!("search restore capture skipped: error={error:?}");let _=error;},
            }
        }
        browser.clear_search_paths()?;
        #[cfg(test)]eprintln!("search restore clear: generation={} pending={} revision={}",self.fast_generation,self.fast_restore.is_some(),browser.search_selection_revision());
        self.fast_found=0;
        if let Some(restore)=&mut self.fast_restore{restore.plan.cleared(browser.search_selection_revision());}
        if self.fast_restore.as_ref().is_some_and(|restore|restore.watch.cancelled.get()){self.discard_fast_restore("input observed during RemoveAll");}
        self.fast_clearing=Some(std::time::Instant::now()+std::time::Duration::from_secs(2));
        Ok(())
    }
    fn finish_fast_clear(&mut self)->windows::core::Result<bool>{
        let Some(deadline)=self.fast_clearing else{return Ok(true);};
        if self.fast_restore.as_ref().is_some_and(|restore|restore.watch.cancelled.get()){self.discard_fast_restore("input observed while clearing view");}
        let browser=self.browser.as_ref().ok_or_else(||windows::core::Error::from_hresult(windows::core::HRESULT(0x8000000Au32 as i32)))?;
        let snapshot=browser.search_items_snapshot();
        match snapshot{
            Ok((items,selected,revision))=>{
                #[cfg(test)]eprintln!("search clear settle: items={items:?} selected={selected:?} revision={revision}");
                if self.fast_restore.as_ref().is_some_and(|restore|!restore.plan.clearing_consistent(&items,&selected)){
                    self.discard_fast_restore("clear transition selection differs from old selection on live rows");
                }
                if items.is_empty(){
                    self.fast_clearing=None;
                    if let Some(restore)=&mut self.fast_restore{restore.plan.expected.clear();restore.plan.before_clear.clear();restore.plan.revision=revision;}
                    return Ok(true);
                }
            },
            Err(error) if error.code().0 as u32==0x8000000A=>{},
            Err(error)=>return Err(error),
        }
        if std::time::Instant::now()>=deadline{return Err(windows::core::Error::new(windows::core::HRESULT(0x800705B4u32 as i32),"搜索结果视图尚未完成清除"));}
        Ok(false)
    }
    fn restore_fast_selection(&mut self){
        if !self.pending_selection_valid(){return;}
        let Some(browser)=&self.browser else{return;};
        let Some(restore)=&mut self.fast_restore else{return;};
        if restore.plan.ready.is_empty(){
            restore.plan.wait_until=None;
            if restore.plan.finished{self.discard_fast_restore("all restoration candidates finished");}return;
        }
        if !restore.plan.wait_allowed(std::time::Instant::now()){
            self.discard_fast_restore("restoration materialization deadline expired");return;
        }
        #[cfg(test)]eprintln!("search restore attempt: pending={:?} expected={} revision={} actual_revision={} cancelled={} visible={:?}",restore.plan.ready,restore.plan.expected.len(),restore.plan.revision,browser.search_selection_revision(),restore.watch.cancelled.get(),browser.item_paths_limit(64));
        let result=browser.restore_search_paths(&restore.plan.ready,restore.plan.revision,||!restore.watch.cancelled.get());
        #[cfg(test)]eprintln!("search restore result: {result:?}");
        let mut keep=true;
        let mut reason="restoration complete";
        if let Ok((restored,revision))=result{
            let restored_keys=selection_keys(&restored);
            let mut expected=restore.plan.expected.clone();expected.extend(restored_keys.iter().cloned());
            restore.plan.expected=expected.clone();restore.plan.revision=revision;restore.plan.confirming=true;
            restore.plan.ready.retain(|path|!restored_keys.contains(&ExplorerBrowserHost::search_path_key(path)));
            if !restored.is_empty(){restore.plan.wait_until=Some(std::time::Instant::now()+std::time::Duration::from_secs(2));}
            match browser.search_selection_snapshot(){
                Ok((selected,revision))=>{
                    if restore.watch.cancelled.get(){reason="input observed after SelectItem";keep=false;}
                    else if selection_keys(&selected)!=expected{
                        #[cfg(test)]eprintln!("search restore post-selection mismatch: selected={selected:?} expected={expected:?}");
                        reason="post-selection snapshot differs from requested selection";keep=false;
                    }
                    else{
                        restore.plan.expected=expected;restore.plan.revision=revision;restore.plan.confirming=false;
                        if restored.is_empty(){restore.plan.retries=restore.plan.retries.saturating_add(1);}else{restore.plan.retries=0;}
                        if restore.plan.ready.is_empty(){restore.plan.wait_until=None;}
                    }
                },
                Err(error)=>{
                    #[cfg(test)]eprintln!("search restore confirmation unavailable: error={error:?}");
                    if restore.watch.cancelled.get(){reason="input observed while confirming SelectItem";keep=false;}
                    else if error.code().0 as u32!=0x8000000A{reason="post-selection snapshot failed";keep=false;}
                },
            }
        }else{reason="restore_search_paths failed";keep=false;}
        if !keep || (restore.plan.finished && !restore.plan.confirming && restore.plan.ready.is_empty()){
            self.discard_fast_restore(reason);
        }else if restore.plan.confirming || !restore.plan.ready.is_empty(){
            unsafe{SetTimer(Some(self.hwnd),TIMER_FAST_SEARCH,30,None);}
        }
    }
    pub(super) fn poll_fast_search(&mut self){
        if self.fast_generation==0 || !matches!(self.current_location(),ShellLocation::Search{..}){
            let events=self.fast_search.as_ref().map(|engine|engine.poll()).unwrap_or_default();
            if let Some(event)=events.last(){
                if event.finished{self.show_message(event.error.as_ref().map_or_else(||"自带索引已就绪".to_owned(),|error|format!("索引未完成：{error}")));}
                else if event.scanning{self.show_message(format!("正在建立自带索引，已处理 {} 项",event.scanned));}
            }return;
        }
        if self.fast_events.is_empty(){if let Some(engine)=&self.fast_search{self.fast_events.extend(engine.poll());}}
        let Some(mut event)=self.fast_events.pop_front()else{self.restore_fast_selection();return;};
        if event.generation==self.fast_generation{
            if self.fast_clearing.is_some(){
                match self.finish_fast_clear(){
                    Ok(true)=>{},
                    Ok(false)=>{self.fast_events.push_front(event);unsafe{SetTimer(Some(self.hwnd),TIMER_FAST_SEARCH,30,None);}return;},
                    Err(error)=>{self.cancel_fast_search();self.show_message(format!("无法刷新搜索结果：{error}"));return;},
                }
            }
            if event.reset || self.fast_reset_pending{
                match self.reset_fast_results(){
                    Ok(())=>{
                        event.reset=false;self.fast_reset_pending=false;self.fast_events.push_front(event);
                        unsafe{SetTimer(Some(self.hwnd),TIMER_FAST_SEARCH,30,None);}return;
                    },
                    Err(error) if error.code().0 as u32==0x8000000A=>{
                        self.fast_events.push_front(event);unsafe{SetTimer(Some(self.hwnd),TIMER_FAST_SEARCH,30,None);}return;
                    },
                    Err(error)=>{self.cancel_fast_search();self.show_message(format!("无法刷新搜索结果：{error}"));return;},
                }
            }
            self.pending_selection_valid();
            if !event.paths.is_empty(){
                if let Some(browser)=&self.browser{match browser.append_search_paths(&event.paths){
                    Ok(added)=>self.fast_found+=added,
                    Err(error) if error.code().0 as u32==0x8000000A=>{
                        self.fast_events.push_front(event);unsafe{SetTimer(Some(self.hwnd),TIMER_FAST_SEARCH,30,None);}return;
                    },
                    Err(error)=>{self.show_message(format!("无法显示搜索结果：{error}"));}
                }}
            }
            // Appending must not turn an external/user selection into our own
            // expected state. Validate it before issuing any restoration calls.
            self.pending_selection_valid();
            if let Some(restore)=&mut self.fast_restore{
                    restore.plan.queue(&event.paths);
                    if event.finished && !restore.plan.finished{
                        let missing=restore.plan.original.clone();restore.plan.queue(&missing);
                        restore.plan.finished=true;restore.plan.retries=0;
                    }
            }
            self.restore_fast_selection();
            self.search_progress=Some(progress_text(&event,self.fast_found));
            self.record_search_progress(&event);
            self.refresh_status();self.relayout();self.position_children();self.invalidate_rect(self.layout.status);
        }
        // Drain small batches after yielding to keyboard, painting and menus.
        if !self.fast_events.is_empty(){post(self.hwnd,WM_FAST_SEARCH,0,0);}
        else if let Some(engine)=&self.fast_search{let events=engine.poll();if !events.is_empty(){self.fast_events.extend(events);post(self.hwnd,WM_FAST_SEARCH,0,0);}}
    }
}
fn progress_text(event:&crate::fast_search::SearchEvent,found:usize)->String{
    use crate::fast_search::BackendKind;
    if event.backend==BackendKind::Everything{
        if event.scanning&&!event.finished&&event.error.is_none(){return "Everything 正在准备文件名索引，完成后自动显示结果…".into();}
        return event.error.as_ref().map_or_else(||format!("共 {} 项 · 当前页 {found} 项 · Everything",event.indexed),|error|format!("搜索未完成：{error}"));
    }
    if !event.scanning&&event.error.is_none(){
        let mut text=format!("共 {} 项 · 当前页 {found} 项 · 自带搜索",event.indexed);
        if let Some(warning)=event.volumes.iter().find_map(|v|v.warning.as_ref()){text.push_str(" · ");text.extend(warning.chars().take(72));}return text;
    }
    let backend=match event.backend{BackendKind::NativeNtfs=>"NTFS 元数据",BackendKind::DirectoryScan=>"目录索引",BackendKind::Mixed=>"NTFS + 目录补全",BackendKind::Everything=>"Everything"};
    let mut text=if let Some(error)=&event.error{format!("搜索未完成：{error}")}
        else if event.truncated{format!("已显示前 {found} 条结果，请补充关键字")}
        else{format!("找到 {found} 条")};
    text.push_str(&format!(" · {backend} · {} · {} 条索引记录",phase_text(event.phase),event.indexed));
    if event.scanning{text.push_str(&format!("（已处理 {} 条）",event.scanned));}
    if event.read_only{text.push_str(" · 共享索引，只读查询");}
    if !event.volumes.is_empty(){
        let volumes=event.volumes.iter().take(8).map(|volume|{
            let root=volume.root.to_string_lossy();let root=root.trim_end_matches(['\\','/']);
            let mode=if volume.backend==BackendKind::DirectoryScan{"目录"}else{"NTFS"};
            format!("{root} {mode}/{}",phase_text(volume.phase))
        }).collect::<Vec<_>>().join("、");
        text.push_str(&format!(" · {volumes}"));
        if event.volumes.len()>8{text.push_str(&format!(" 等 {} 卷",event.volumes.len()));}
        if let Some(warning)=event.volumes.iter().find_map(|volume|volume.warning.as_ref()){
            let short:String=warning.chars().take(72).collect();text.push_str(&format!(" · {short}"));
            if warning.chars().count()>72{text.push('…');}
        }
    }
    if event.denied>0{text.push_str(&format!(" · 跳过 {} 处受限位置",event.denied));}
    if event.skipped_reparse>0{text.push_str(&format!(" · {} 处目录链接未跟随",event.skipped_reparse));}
    text
}
fn phase_text(phase:crate::fast_search::IndexPhase)->&'static str{
    use crate::fast_search::IndexPhase;
    match phase{IndexPhase::Starting=>"准备中",IndexPhase::Snapshot=>"建立快照",IndexPhase::CatchingUp=>"同步 USN",
        IndexPhase::Scanning=>"目录补全",IndexPhase::Attributes=>"读取匹配项属性",IndexPhase::Ready=>"就绪",IndexPhase::Paused=>"已暂停"}
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn new_input_cancels_old_choices_even_when_selection_did_not_change(){
        let paths=vec![PathBuf::from(r"C:\files\a.txt")];let plan=SelectionPlan::new(4,paths.clone(),8);
        assert!(plan.consistent(4,false,&paths));assert!(!plan.consistent(4,true,&paths));
        assert!(!plan.consistent(5,false,&paths));assert!(!plan.consistent(4,false,&[PathBuf::from(r"C:\files\b.txt")]));
    }
    #[test]fn restoration_candidates_require_the_whole_exact_path(){
        let a=PathBuf::from(r"C:\first\same.txt");let mut plan=SelectionPlan::new(2,vec![a.clone()],1);plan.cleared(2);
        plan.queue(&[PathBuf::from(r"C:\other\same.txt"),PathBuf::from(r"C:\first\SAME.txt")]);assert!(plan.ready.is_empty());
        plan.queue(&[a.clone(),a.clone()]);assert_eq!(plan.ready,vec![a]);
    }
    #[test]fn repeated_reset_keeps_unreturned_choices_without_navigation(){
        let a=PathBuf::from(r"C:\files\a.txt");let b=PathBuf::from(r"C:\files\b.txt");
        let mut plan=SelectionPlan::new(7,vec![a.clone(),b.clone()],1);plan.cleared(2);plan.queue(&[a]);
        plan.cleared(3);assert_eq!(plan.generation,7);assert_eq!(plan.original.len(),2);assert!(plan.ready.is_empty());
        plan.queue(&[b.clone()]);assert_eq!(plan.ready,vec![b]);
    }
    #[test]fn data_and_paint_notifications_do_not_count_as_user_input(){
        assert!(selection_input(WM_LBUTTONDOWN));assert!(selection_input(WM_KEYDOWN));
        assert!(!selection_input(WM_PAINT));assert!(!selection_input(WM_FAST_SEARCH));assert!(!selection_input(WM_SHELL_SELECTION_CHANGED));
    }
    #[test]fn clear_transition_only_accepts_old_selection_on_rows_still_present(){
        let a=PathBuf::from(r"C:\files\a.txt");let b=PathBuf::from(r"C:\files\b.txt");let c=PathBuf::from(r"C:\files\c.txt");
        let mut plan=SelectionPlan::new(1,vec![a.clone(),b.clone()],1);plan.cleared(2);
        assert!(plan.clearing_consistent(&[a.clone(),b.clone(),c.clone()],&[a.clone(),b]));
        assert!(plan.clearing_consistent(&[a.clone(),c.clone()],&[a.clone()]));
        assert!(plan.clearing_consistent(&[],&[]));
        assert!(!plan.clearing_consistent(&[a.clone(),c.clone()],&[c]));
        assert!(!plan.clearing_consistent(&[a],&[]));
    }
    #[test]fn materialization_wait_uses_elapsed_time_not_validation_call_count(){
        let mut plan=SelectionPlan::new(1,vec![PathBuf::from(r"C:\files\a.txt")],1);
        let now=std::time::Instant::now();
        for _ in 0..1000{assert!(plan.wait_allowed(now));}
        assert!(plan.wait_allowed(now+std::time::Duration::from_millis(1999)));
        assert!(!plan.wait_allowed(now+std::time::Duration::from_secs(2)));
        plan.cleared(2);assert!(plan.wait_allowed(now+std::time::Duration::from_secs(3)));
    }
    #[test]fn progress_identifies_backend_phase_shared_role_and_index_records(){
        use crate::fast_search::{BackendKind,IndexPhase,SearchEvent,VolumeStatus};
        let event=SearchEvent{generation:1,paths:Vec::new(),scanned:12,indexed:25,denied:0,skipped_reparse:0,
            scanning:true,finished:false,truncated:false,error:None,reset:true,backend:BackendKind::Mixed,
            phase:IndexPhase::CatchingUp,volumes:vec![VolumeStatus{root:PathBuf::from(r"C:\"),backend:BackendKind::NativeNtfs,
                phase:IndexPhase::Ready,indexed:20,checkpoint:Some(10),warning:None}],read_only:true};
        let text=progress_text(&event,2);for part in ["NTFS","USN","只读","索引记录","C:"]{assert!(text.contains(part));}
        assert!(!text.contains("唯一文件"));
    }
}
