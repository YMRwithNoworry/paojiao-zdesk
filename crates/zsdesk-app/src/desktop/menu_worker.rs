//! One bounded STA owns desktop Shell menus, including their command handlers.
//! Only PIDL bytes and application command IDs cross the desktop UI boundary.
use std::{cell::{Cell,RefCell},collections::VecDeque,path::{Path,PathBuf},sync::{Arc,Mutex,atomic::{AtomicBool,AtomicIsize,AtomicU64,Ordering},mpsc},thread::{self,JoinHandle},time::{Duration,Instant}};
use std::os::windows::fs::MetadataExt;
use windows::{core::{w,PCWSTR},Win32::{Foundation::{HINSTANCE,HWND,LPARAM,LRESULT,POINT,WPARAM},System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize,OleFlushClipboard},Threading::GetCurrentThreadId,DataExchange::GetClipboardOwner},UI::{Input::KeyboardAndMouse::{SetActiveWindow,SetFocus},WindowsAndMessaging::*}}};
use zsdesk_ui::menus::{MenuItemSpec,MenuSpec};
use zsdesk_windows::shell::menu::{forward_menu_message,MenuInterceptions,MenuOutcome,ShellMenu};

const CLASS:PCWSTR=w!("ZSDesk.Desktop.MenuOwner");
const CANCEL_TIMER:usize=1;
const CANCEL_REQUEST:u32=WM_APP+25;
static RETIRING:Mutex<Vec<JoinHandle<()>>>=Mutex::new(Vec::new());
pub(super) fn retiring_count()->usize{
    let mut threads=RETIRING.lock().unwrap_or_else(|e|e.into_inner());
    let mut index=0;while index<threads.len(){if threads[index].is_finished(){let _=threads.swap_remove(index).join();}else{index+=1;}}threads.len()
}
pub(super) const READY:u32=WM_APP+24;
#[derive(Clone)]
pub(super) enum Target{Items{pidls:Vec<Vec<u8>>,key:Option<String>},Background{folder:Option<PathBuf>}}
#[derive(Clone)]
pub(super) struct Request{
    pub fence:u64,pub owner:isize,pub point:POINT,pub profile:PathBuf,pub items:Vec<MenuItemSpec>,pub target:Target,pub extended:bool,
    pub generation:u64,pub queued:Instant,pub input_started:Instant,pub ui_thread:u32,pub foreground:isize,
    pub mode:crate::context_menu_process::Mode,
    #[cfg(test)] pub probe:Option<Probe>,
}
impl Request{
    pub fn new(fence:u64,owner:HWND,point:POINT,profile:PathBuf,items:Vec<MenuItemSpec>,target:Target,extended:bool)->Self{
        let now=Instant::now();Self{fence,owner:owner.0 as isize,point,profile,items,target,extended,generation:0,queued:now,input_started:now,ui_thread:unsafe{GetCurrentThreadId()},foreground:unsafe{GetForegroundWindow()}.0 as isize,mode:crate::context_menu_process::Mode::More,#[cfg(test)]probe:None}
    }
}
#[derive(Clone,Debug,Default,serde::Serialize)]
pub(super) struct Timing{
    pub generation:u64,pub ui_thread:u32,pub menu_thread:u32,pub owner_thread:u32,
    pub queue_microseconds:u64,pub bind_microseconds:u64,pub populate_microseconds:u64,
    pub input_to_menu_microseconds:Option<u64>,pub result:String,
    pub extension_process:Option<u32>,
    pub initial_validation_microseconds:u64,pub settings_microseconds:u64,pub request_snapshot_microseconds:u64,
    pub settings_cache_hit:bool,
    pub isolation:crate::context_menu_process::MenuTimings,
}
struct ValidationEvent(usize);
impl Drop for ValidationEvent{fn drop(&mut self){let _=unsafe{windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(self.0 as*mut _))};}}
pub(super) struct ValidationAnswer{sender:mpsc::SyncSender<bool>,wake:Arc<ValidationEvent>}
impl ValidationAnswer{
    fn wake(&self){let _=unsafe{windows::Win32::System::Threading::SetEvent(windows::Win32::Foundation::HANDLE(self.wake.0 as*mut _))};}
    pub fn send(&self,value:bool)->Result<(),mpsc::SendError<bool>>{let result=self.sender.send(value);self.wake();result}
    pub fn try_send(&self,value:bool)->Result<(),mpsc::TrySendError<bool>>{let result=self.sender.try_send(value);self.wake();result}
}
impl Drop for ValidationAnswer{fn drop(&mut self){self.wake();}}
pub(super) enum Event{
    Validate{request:Request,before_invoke:bool,answer:ValidationAnswer},
    Finished{request:Request,ids:Vec<String>,outcome:Result<MenuOutcome,(i32,String)>,timing:Timing},
}
struct Shared{
    pending:Mutex<Option<Request>>,events:Mutex<VecDeque<Event>>,stop:AtomicBool,generation:AtomicU64,window:AtomicIsize,
    destination:isize,failure:Mutex<Option<(i32,String)>>,
}
impl Shared{
    fn current(&self,generation:u64)->bool{!self.stop.load(Ordering::Acquire)&&self.generation.load(Ordering::Acquire)==generation}
    fn push(&self,event:Event){
        self.events.lock().unwrap_or_else(|e|e.into_inner()).push_back(event);
        let _=unsafe{PostMessageW(Some(HWND(self.destination as*mut _)),READY,WPARAM(0),LPARAM(0))};
    }
    fn wake(&self,cancel:bool){
        let raw=self.window.load(Ordering::Acquire);if raw==0{return;}
        let window=HWND(raw as*mut _);
        if cancel{let _=unsafe{PostMessageW(Some(window),CANCEL_REQUEST,WPARAM(0),LPARAM(0))};}
        let _=unsafe{PostMessageW(Some(window),WM_NULL,WPARAM(0),LPARAM(0))};
    }
}
pub(super) struct Worker{shared:Arc<Shared>,thread:Option<JoinHandle<()>>,recent:VecDeque<Timing>}
impl Worker{
    pub fn new(destination:HWND)->anyhow::Result<Self>{Self::start(destination,None)}
    fn start(destination:HWND,desktop:Option<usize>)->anyhow::Result<Self>{
        let _=retiring_count();
        // A newly-created thread otherwise attaches to the process default
        // desktop. Match its UI owner before COM/window creation, including
        // hosts running on a different Windows desktop.
        let desktop=desktop.or_else(||{let value=unsafe{GetThreadDesktop(GetCurrentThreadId())};(!value.is_null()).then_some(value as usize)});
        let shared=Arc::new(Shared{pending:Mutex::new(None),events:Mutex::new(VecDeque::new()),stop:AtomicBool::new(false),generation:AtomicU64::new(0),window:AtomicIsize::new(0),destination:destination.0 as isize,failure:Mutex::new(None)});
        let worker=shared.clone();
        let thread=thread::Builder::new().name("zsdesk-shell-menu".into()).spawn(move||{
            let result=(||->windows::core::Result<()>{
                if let Some(desktop)=desktop{if unsafe{SetThreadDesktop(desktop as*mut core::ffi::c_void)}==0{return Err(windows::core::Error::from_thread());}}
                run(worker.clone())
            })();
            if let Err(error)=result{
                *worker.failure.lock().unwrap_or_else(|e|e.into_inner())=Some((error.code().0,error.message()));
                if let Some(request)=worker.pending.lock().unwrap_or_else(|e|e.into_inner()).take(){worker.push(Event::Finished{request,ids:Vec::new(),outcome:Err((error.code().0,error.message())),timing:Timing::default()});}
            }
        })?;
        Ok(Self{shared,thread:Some(thread),recent:VecDeque::new()})
    }
    pub fn submit(&self,mut request:Request){
        request.generation=self.shared.generation.fetch_add(1,Ordering::AcqRel).wrapping_add(1);
        request.queued=Instant::now();
        let mut pending=self.shared.pending.lock().unwrap_or_else(|e|e.into_inner());
        if let Some(error)=self.shared.failure.lock().unwrap_or_else(|e|e.into_inner()).clone(){drop(pending);self.shared.push(Event::Finished{request,ids:Vec::new(),outcome:Err(error),timing:Timing::default()});return;}
        *pending=Some(request);drop(pending);self.shared.wake(true);
    }
    pub fn drain(&mut self)->Vec<Event>{
        let events:Vec<_>=self.shared.events.lock().unwrap_or_else(|e|e.into_inner()).drain(..).collect();
        for event in &events{if let Event::Finished{timing,..}=event{self.recent.push_back(timing.clone());if self.recent.len()>8{self.recent.pop_front();}}}events
    }
    pub fn is_current(&self,generation:u64)->bool{self.shared.current(generation)}
    pub fn cancel_current(&self){self.shared.generation.fetch_add(1,Ordering::AcqRel);self.shared.pending.lock().unwrap_or_else(|error|error.into_inner()).take();self.shared.wake(true);}
    pub fn diagnostics(&self)->serde_json::Value{serde_json::json!({"worker_thread_running":self.thread.as_ref().is_some_and(|thread|!thread.is_finished()),"retiring_threads":retiring_count(),"latest_generation":self.shared.generation.load(Ordering::Acquire),"recent":self.recent})}
    pub fn stop(&self){self.shared.stop.store(true,Ordering::Release);self.shared.pending.lock().unwrap_or_else(|e|e.into_inner()).take();self.shared.wake(true);}
    pub fn shutdown(&mut self){
        self.stop();
        // Shell extensions can synchronously send their owner messages. Keep
        // the caller's apartment pumping while its dedicated STA is retiring.
        if let Some(thread)=self.thread.take(){
            let deadline=Instant::now()+Duration::from_secs(2);
            while !thread.is_finished(){
                for event in self.drain(){if let Event::Validate{answer,..}=event{let _=answer.try_send(false);}}
                let mut message=MSG::default();
                for _ in 0..64{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}
                thread::sleep(Duration::from_millis(5));
                if Instant::now()>=deadline{break;}
            }
            if thread.is_finished(){let _=thread.join();}else{
                // A third-party COM call cannot be cancelled by terminating its
                // thread. Retain its identity until it returns; desktop close
                // itself must not wait forever for an extension or its dialog.
                RETIRING.lock().unwrap_or_else(|e|e.into_inner()).push(thread);
            }
        }
        self.shared.events.lock().unwrap_or_else(|e|e.into_inner()).clear();
    }
}
impl Drop for Worker{fn drop(&mut self){self.shutdown();}}
struct Apartment;
impl Drop for Apartment{fn drop(&mut self){
    // Preserve delayed-rendered Shell clipboard data when this STA retires.
    let owner=unsafe{GetClipboardOwner()}.unwrap_or_default();
    if !owner.is_invalid()&&unsafe{GetWindowThreadProcessId(owner,None)}==unsafe{GetCurrentThreadId()}{let _=unsafe{OleFlushClipboard()};}
    unsafe{OleUninitialize();}
}}
struct Owner(HWND);
impl Drop for Owner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
#[derive(Default)]
struct MenuPolicyCache{
    entry:Option<(PathBuf,(u64,u64,u64),zsdesk_windows::shell::menu::ContextMenuOptions)>,
    #[cfg(test)]reads:u64,
}
impl MenuPolicyCache{
    fn stamp(profile:&Path)->Option<(u64,u64,u64)>{
        let metadata=std::fs::metadata(profile).ok()?;
        metadata.is_file().then(||(metadata.len(),metadata.last_write_time(),metadata.creation_time()))
    }
    fn options(&mut self,profile:&Path)->(zsdesk_windows::shell::menu::ContextMenuOptions,bool){
        let stamp=Self::stamp(profile);
        if let Some((cached_path,cached_stamp,options))=&self.entry{
            if cached_path==profile&&Some(*cached_stamp)==stamp{return (options.clone(),true);}
        }
        #[cfg(test)]{self.reads+=1;}
        let loaded=crate::storage::load(profile).map(|session|crate::context_preferences::native(&session.ui.context_menu));
        // Cache only one verified snapshot on this menu STA. Session navigation
        // can enlarge the profile substantially, but unchanged right-clicks
        // need only its metadata; cross-window preference edits invalidate it.
        self.entry=None;
        match loaded{
            Ok(options)=>{
                if stamp.is_some()&&Self::stamp(profile)==stamp{self.entry=Some((profile.into(),stamp.unwrap(),options.clone()));}
                (options,false)
            },Err(_)=>(Default::default(),false),
        }
    }
}
struct WindowState{shared:Arc<Shared>,generation:Cell<u64>,source:Cell<isize>,entered:Cell<Option<Instant>>,policy:RefCell<MenuPolicyCache>,#[cfg(test)]probe:Cell<Option<Probe>>}
unsafe extern "system" fn procedure(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if message==WM_NCCREATE{let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize);}return LRESULT(1);}
    let state=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const WindowState;
    if !state.is_null(){let state=unsafe{&*state};
        if message==WM_ENTERMENULOOP{state.entered.set(Some(Instant::now()));#[cfg(test)]if state.probe.get().is_some(){unsafe{SetTimer(Some(hwnd),2,40,None);}}}
        if message==WM_CANCELMODE||((message==CANCEL_REQUEST||(message==WM_TIMER&&wp.0==CANCEL_TIMER))&&(!state.shared.current(state.generation.get())||!unsafe{IsWindow(Some(HWND(state.source.get()as*mut _)))}.as_bool())){let _=unsafe{EndMenu()};return LRESULT(0);}
        #[cfg(test)]if message==WM_TIMER&&wp.0==2{let _=unsafe{KillTimer(Some(hwnd),2)};if let Some(probe)=state.probe.get(){let _=unsafe{PostMessageW(Some(hwnd),WM_CHAR,WPARAM((if probe.copy{b'c'}else{b'z'})as usize),LPARAM(1))};}return LRESULT(0);}
    }
    if let Some(result)=forward_menu_message(message,wp,lp){return result;}
    unsafe{DefWindowProcW(hwnd,message,wp,lp)}
}
fn error_abort()->windows::core::Error{windows::core::Error::from_hresult(windows::core::HRESULT(0x80004004u32 as i32))}
fn validate(shared:&Shared,request:&Request,before_invoke:bool)->windows::core::Result<()>{
    if !shared.current(request.generation){return Err(error_abort());}
    let event=unsafe{windows::Win32::System::Threading::CreateEventW(None,false,false,None)}?;
    let wake=Arc::new(ValidationEvent(event.0 as usize));
    let (sender,receiver)=mpsc::sync_channel(1);shared.push(Event::Validate{request:request.clone(),before_invoke,answer:ValidationAnswer{sender,wake:wake.clone()}});
    let deadline=Instant::now()+Duration::from_secs(2);
    loop{
        match receiver.try_recv(){Ok(true)if shared.current(request.generation)=>return Ok(()),Ok(_)|Err(mpsc::TryRecvError::Disconnected)=>return Err(error_abort()),Err(mpsc::TryRecvError::Empty)=>{}}
        if !shared.current(request.generation)||Instant::now()>=deadline{return Err(error_abort());}
        let mut message=MSG::default();
        for _ in 0..32{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message==WM_QUIT{shared.stop.store(true,Ordering::Release);return Err(error_abort());}unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}
        let _=crate::context_menu_process::wait_signal(&[wake.0],deadline.saturating_duration_since(Instant::now()).as_millis().min(100)as u32);
    }
}
fn foreground_allowed(expected:isize)->bool{unsafe{GetForegroundWindow()}.0 as isize==expected}
fn micros(duration:Duration)->u64{duration.as_micros().min(u64::MAX as u128)as u64}
#[cfg(test)]
fn legacy_probe(shared:&Shared,state:&WindowState,owner:HWND,request:&Request,timing:&mut Timing,ids:&mut Vec<String>)->windows::core::Result<MenuOutcome>{
    state.generation.set(request.generation);state.source.set(request.owner);state.entered.set(None);
    #[cfg(test)]{state.probe.set(request.probe);if let Some(probe)=request.probe{thread::sleep(Duration::from_millis(probe.delay_ms));}}
    if !shared.current(request.generation){return Err(error_abort());}
    crate::context_preferences::apply_from_profile(&request.profile);
    let started=Instant::now();
    let shell=match &request.target{
        Target::Items{pidls,..}=>Some(ShellMenu::for_items(owner,pidls)?),
        Target::Background{folder:Some(path)}=>{let folder=zsdesk_windows::desktop::folder_for_path(path)?;Some(ShellMenu::for_folder_background(owner,&folder)?)},
        Target::Background{folder:None}=>None,
    };
    timing.bind_microseconds=micros(started.elapsed());
    if let Some(menu)=&shell{
        let started=Instant::now();menu.populate(matches!(&request.target,Target::Items{key:Some(_),..}),request.extended)?;timing.populate_microseconds=micros(started.elapsed());
        #[cfg(test)]if request.probe.is_some(){
            let count=unsafe{GetMenuItemCount(Some(menu.hmenu()))};let copy=(0..count).map(|index|unsafe{GetMenuItemID(menu.hmenu(),index)}).find(|id|menu.verb(*id).is_some_and(|verb|verb.eq_ignore_ascii_case("copy"))).ok_or_else(error_abort)?;
            for index in (0..count).rev(){if unsafe{GetMenuItemID(menu.hmenu(),index)}!=copy{unsafe{DeleteMenu(menu.hmenu(),index as u32,MF_BYPOSITION)?;}}}
        }
        *ids=super::insert_top(menu.hmenu(),&request.items);
    }
    validate(shared,request,false)?;
    let source=HWND(request.owner as*mut _);
    #[cfg(test)]let isolated=request.probe.is_some();
    #[cfg(not(test))]let isolated=false;
    if !isolated&&!foreground_allowed(request.foreground){return Err(error_abort());}
    unsafe{let _=SetWindowPos(owner,None,request.point.x,request.point.y,0,0,SWP_NOZORDER|SWP_NOACTIVATE);let _=ShowWindow(owner,SW_SHOWNOACTIVATE);let _=SetForegroundWindow(owner);let _=SetActiveWindow(owner);let _=SetFocus(Some(owner));SetTimer(Some(owner),CANCEL_TIMER,100,None);}
    let result=match (&shell,&request.target){
        (Some(menu),Target::Items{pidls,key})=>menu.track_selected_before(owner,request.point,pidls,ids.len(),MenuInterceptions{rename:key.is_some(),open:pidls.len()==1&&key.is_some()},&mut||validate(shared,request,true)),
        (Some(menu),Target::Background{folder:Some(path)})=>menu.track_folder_background(owner,request.point,path,ids.len(),&mut||validate(shared,request,true)),
        _=>{let spec=MenuSpec{items:request.items.clone(),..Default::default()};let selected=super::popup(owner,&spec,request.point);*ids=selected.clone().into_iter().collect();Ok(selected.map_or(MenuOutcome::Dismissed,|_|MenuOutcome::Application(zsdesk_windows::shell::menu::APP_FIRST)))},
    };
    let _=unsafe{KillTimer(Some(owner),CANCEL_TIMER)};
    timing.input_to_menu_microseconds=state.entered.get().map(|entered|micros(entered.saturating_duration_since(request.input_started)));
    // Keep the zero-area owner alive for modeless Shell dialogs and clipboard
    // rendering; hiding/destroying it here would also hide its owned dialogs.
    if unsafe{GetForegroundWindow()}==owner&&unsafe{IsWindow(Some(source))}.as_bool(){let _=unsafe{SetForegroundWindow(source)};}
    result
}
fn perform(shared:&Shared,state:&WindowState,owner:HWND,request:&Request,timing:&mut Timing,ids:&mut Vec<String>)->windows::core::Result<MenuOutcome>{
    // The old same-STA probe is retained only to exercise public menu message
    // forwarding. Production extension code is always in a child process.
    #[cfg(test)]if request.probe.is_some(){return legacy_probe(shared,state,owner,request,timing,ids);}
    let _=(state,owner,ids);
    let stage=Instant::now();
    validate(shared,request,false)?;
    timing.initial_validation_microseconds=micros(stage.elapsed());
    if !foreground_allowed(request.foreground){return Err(windows::core::Error::new(windows::core::HRESULT(0x80004004u32 as i32),"菜单打开前前台窗口已改变"));}
    let stage=Instant::now();
    let (options,cache_hit)=state.policy.borrow_mut().options(&request.profile);
    timing.settings_cache_hit=cache_hit;
    timing.settings_microseconds=micros(stage.elapsed());let stage=Instant::now();
    let(pidls,folder,intercept)=match &request.target{
        Target::Items{pidls,key}=>(pidls.clone(),None,MenuInterceptions{rename:key.is_some(),open:pidls.len()==1&&key.is_some()}),
        Target::Background{folder}=>(Vec::new(),folder.clone(),MenuInterceptions::default()),
    };
    timing.request_snapshot_microseconds=micros(stage.elapsed());
    let input_to_client=micros(request.input_started.elapsed());
    let mut client=crate::context_menu_process::Client::spawn_extended(HWND(request.owner as*mut _),request.point,pidls,folder,options,intercept,request.mode,request.extended)
        .map_err(|error|windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32),error.to_string()))?;
    timing.extension_process=client.pid();
    loop{
        if !shared.current(request.generation){client.cancel();return Err(error_abort());}
        if let Some(result)=client.poll(&mut|before|validate(shared,request,before).is_ok()&&(before||foreground_allowed(request.foreground))){
            if let Some((bind,populate))=client.timings(){timing.bind_microseconds=bind;timing.populate_microseconds=populate;}
            timing.isolation=client.detailed_timings();
            timing.input_to_menu_microseconds=timing.isolation.parent_to_menu_microseconds.map(|time|time.saturating_add(input_to_client));
            return result.map_err(|error|windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32),error.to_string()));
        }
        let mut message=MSG::default();for _ in 0..32{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
            if message.message==WM_QUIT{client.cancel();return Err(error_abort());}unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}
        client.wait_for_progress(100);
    }
}
fn run(shared:Arc<Shared>)->windows::core::Result<()>{
    unsafe{OleInitialize(None)?;}let _apartment=Apartment;
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(procedure),lpszClassName:CLASS,..Default::default()});}
    let state=Box::new(WindowState{shared:shared.clone(),generation:Cell::new(0),source:Cell::new(0),entered:Cell::new(None),policy:RefCell::new(MenuPolicyCache::default()),#[cfg(test)]probe:Cell::new(None)});
    let owner=Owner(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW,CLASS,w!("ZSdesk"),WS_POPUP,0,0,0,0,None,None,Some(instance),Some((&*state as*const WindowState).cast()))?});
    shared.window.store(owner.0.0 as isize,Ordering::Release);
    while !shared.stop.load(Ordering::Acquire){
        let next=shared.pending.lock().unwrap_or_else(|e|e.into_inner()).take();
        if let Some(request)=next{
            let mut timing=Timing{generation:request.generation,ui_thread:request.ui_thread,menu_thread:unsafe{GetCurrentThreadId()},owner_thread:unsafe{GetWindowThreadProcessId(owner.0,None)},queue_microseconds:micros(request.queued.elapsed()),..Default::default()};let mut ids=Vec::new();
            let outcome=perform(&shared,&state,owner.0,&request,&mut timing,&mut ids);
            timing.result=match &outcome{Ok(value)=>format!("{value:?}"),Err(error)=>format!("0x{:08x}: {}",error.code().0 as u32,error.message())};
            shared.push(Event::Finished{request,ids,outcome:outcome.map_err(|error|(error.code().0,error.message())),timing});continue;
        }
        let mut message=MSG::default();let status=unsafe{GetMessageW(&mut message,None,0,0)}.0;if status<=0{break;}unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}
    }
    crate::context_menu_process::shutdown_idle_worker_for_thread();
    shared.window.store(0,Ordering::Release);drop(owner);Ok(())
}

#[cfg(test)]#[derive(Clone,Copy)]pub(super) struct Probe{pub delay_ms:u64,pub copy:bool}
#[link(name="user32")]unsafe extern "system"{fn SetThreadDesktop(desktop:*mut core::ffi::c_void)->i32;fn GetThreadDesktop(thread:u32)->*mut core::ffi::c_void;}

#[cfg(test)]mod tests{
    use super::*;
    use anyhow::{Context,Result,ensure};
    use windows::Win32::System::{DataExchange::GetClipboardSequenceNumber,Ole::{OleGetClipboard,OleSetClipboard}};
    #[test]
    fn menu_policy_cache_reuses_unchanged_profiles_and_observes_external_edits()->Result<()>{
        let root=std::env::temp_dir().join(format!("zsdesk-menu-policy-{}",std::process::id()));
        std::fs::create_dir(&root)?;
        struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
        let _fixture=Fixture(root.clone());let first=root.join("first.json");let second=root.join("second.json");
        let mut session=crate::storage::default_session();session.ui.context_menu.show_wps=false;
        crate::storage::save(&first,&session)?;
        session.ui.context_menu.show_wps=true;crate::storage::save(&second,&session)?;
        let mut cache=MenuPolicyCache::default();
        let (initial,hit)=cache.options(&first);ensure!(!initial.show_wps&&!hit&&cache.reads==1,"initial policy was not loaded");
        for _ in 0..4{let (options,hit)=cache.options(&first);ensure!(!options.show_wps&&hit,"unchanged menu policy was reparsed");}
        ensure!(cache.reads==1,"unchanged profile reparsed");
        let (options,hit)=cache.options(&second);ensure!(options.show_wps&&!hit,"a different profile reused the first policy");
        let (options,hit)=cache.options(&first);ensure!(!options.show_wps&&!hit,"bounded cache did not switch back");
        crate::storage::save(&first,&session)?;
        let (options,hit)=cache.options(&first);ensure!(options.show_wps&&!hit,"external preference edit was hidden by the cache");
        std::fs::write(&first,b"invalid session")?;
        let (options,hit)=cache.options(&first);ensure!(!options.show_wps&&!hit&&cache.entry.is_none(),"invalid configuration retained stale policy");
        crate::storage::save(&first,&session)?;
        let (options,hit)=cache.options(&first);ensure!(options.show_wps&&!hit,"restored configuration was not reloaded");
        Ok(())
    }
    #[link(name="user32")]unsafe extern "system"{
        fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const core::ffi::c_void,flags:u32,access:u32,attributes:*const core::ffi::c_void)->*mut core::ffi::c_void;
        fn CloseDesktop(desktop:*mut core::ffi::c_void)->i32;
    }
    thread_local!{static HEARTBEATS:Cell<u32>=const{Cell::new(0)};}
    unsafe extern "system" fn ui_proc(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
        if message==WM_TIMER{HEARTBEATS.with(|beats|beats.set(beats.get()+1));return LRESULT(0);}
        unsafe{DefWindowProcW(hwnd,message,wp,lp)}
    }
    struct Clipboard{previous:Option<windows::Win32::System::Com::IDataObject>,ours:Option<u32>}
    impl Drop for Clipboard{fn drop(&mut self){if self.ours==Some(unsafe{GetClipboardSequenceNumber()}){let _=unsafe{OleSetClipboard(self.previous.as_ref())};}}}
    fn exercise(root:&std::path::Path,desktop:usize)->Result<serde_json::Value>{
        unsafe{OleInitialize(None)?;}let _apartment=Apartment;
        let fixture=root.join("异步 右键.txt");std::fs::write(&fixture,b"ZSdesk asynchronous menu fixture")?;
        let pidl=zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(fixture.clone())).context("fixture PIDL")?;
        let instance=HINSTANCE(unsafe{GetModuleHandleW(None)?}.0);
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(ui_proc),lpszClassName:w!("ZSDesk.MenuWorker.Heartbeat"),..Default::default()});}
        let ui=Owner(unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),w!("ZSDesk.MenuWorker.Heartbeat"),w!("Menu STA regression"),WS_OVERLAPPEDWINDOW,20,20,400,300,None,None,Some(instance),None)?});
        unsafe{let _=ShowWindow(ui.0,SW_SHOW);SetTimer(Some(ui.0),1,20,None);}
        let mut clipboard=Clipboard{previous:unsafe{OleGetClipboard()}.ok(),ours:None};
        let mut worker=Worker::start(ui.0,Some(desktop))?;let mut rows=Vec::new();
        for copy in [false,true]{
            HEARTBEATS.with(|beats|beats.set(0));
            let mut request=Request::new(1,ui.0,POINT{x:60,y:80},root.join("absent-profile.json"),vec![zsdesk_ui::menus::entry("应用命令(&Z)","probe",true,false)],Target::Items{pidls:vec![pidl.clone()],key:None},false);
            request.probe=Some(Probe{delay_ms:400,copy});let submitted=Instant::now();worker.submit(request);let submit_us=micros(submitted.elapsed());
            ensure!(submit_us<100_000,"UI submit took {submit_us}us");
            let deadline=Instant::now()+Duration::from_secs(15);let mut completed=None;let mut before_menu_beats=0;
            while completed.is_none()&&Instant::now()<deadline{
                for event in worker.drain(){match event{
                    Event::Validate{answer,..}=>{before_menu_beats=HEARTBEATS.with(Cell::get);let _=answer.send(true);},
                    Event::Finished{outcome,timing,..}=>completed=Some((outcome,timing)),
                }}
                let mut message=MSG::default();for _ in 0..64{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}
                thread::sleep(Duration::from_millis(2));
            }
            let (outcome,timing)=completed.context("menu worker timed out")?;
            let outcome=outcome.map_err(|(code,message)|anyhow::anyhow!("{code:#x}: {message}"))?;
            ensure!(timing.menu_thread!=timing.ui_thread&&timing.menu_thread==timing.owner_thread,"menu COM/owner apartment mismatch");
            ensure!(before_menu_beats>=10,"desktop UI stopped pumping during menu construction ({before_menu_beats} beats)");
            ensure!(timing.input_to_menu_microseconds.is_some(),"real native popup did not enter its menu loop");
            if copy{
                ensure!(outcome==MenuOutcome::ShellInvoked,"wrong Shell outcome: {outcome:?}");
                let copied=unsafe{OleGetClipboard()}?;let paths=zsdesk_windows::ole::hdrop_paths(&copied);
                if paths==vec![fixture.clone()]{clipboard.ours=Some(unsafe{GetClipboardSequenceNumber()});}
                ensure!(paths==vec![fixture.clone()],"Shell copy used a different item");
            }else{ensure!(outcome==MenuOutcome::Application(zsdesk_windows::shell::menu::APP_FIRST),"application command misrouted: {outcome:?}");}
            rows.push(serde_json::json!({"copy":copy,"injected_construction_delay_ms":400,"submit_microseconds":submit_us,"ui_heartbeats_before_popup":before_menu_beats,"timing":timing}));
        }
        // Restore before shutdown: flushing this worker's delayed clipboard can
        // legitimately change its sequence number and defeat a later guard.
        drop(clipboard);
        let restored_sequence=unsafe{GetClipboardSequenceNumber()};
        let closing=Instant::now();worker.shutdown();ensure!(closing.elapsed()<Duration::from_secs(2),"normal worker shutdown was delayed");ensure!(retiring_count()==0,"menu thread did not retire");
        ensure!(unsafe{GetClipboardSequenceNumber()}==restored_sequence,"worker changed the restored clipboard");
        ensure!(std::fs::read(&fixture)?==b"ZSdesk asynchronous menu fixture","fixture content changed");
        drop(ui);
        Ok(serde_json::json!({"cases":rows,"worker_joined":true,"shell_copy_paths_verified":true,"fixture_unchanged":true,"scope":"legacy public-menu STA message forwarding, not production process isolation"}))
    }
    #[test]
    #[ignore="Native isolated desktop test; requires ZSDESK_NATIVE_TEST_OUTPUT and preserves/restores clipboard"]
    fn native_menu_worker_keeps_desktop_responsive_and_dispatches_exact_commands()->Result<()>{
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("menu-worker");ensure!(root.is_absolute(),"absolute output required");std::fs::create_dir_all(&root)?;
        let foreground=unsafe{GetForegroundWindow()};let name:Vec<u16>=format!("ZSDesk.MenuWorker.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
        let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"CreateDesktop failed");
        let raw=desktop as usize;let output=root.clone();
        let result=thread::spawn(move||->Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut _)}!=0,"SetThreadDesktop failed");exercise(&output,raw)}).join().map_err(|_|anyhow::anyhow!("menu worker test panicked")).and_then(|value|value);
        let closed=unsafe{CloseDesktop(desktop)}!=0;let unchanged=foreground==unsafe{GetForegroundWindow()};
        let report=serde_json::json!({"state":if result.is_ok()&&closed&&unchanged{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"foreground_unchanged":unchanged});
        std::fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&unchanged,"desktop cleanup/focus changed");Ok(())
    }
}
