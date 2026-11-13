//! Process-isolated, time-bounded visibility leases for new Explorer windows.
//! This process deliberately does not call Shell COM: a blocked Shell query in
//! the forwarding thread must never strand an invisible source window.
use std::{cell::RefCell,collections::{HashMap,HashSet},io::{BufRead,BufReader,Write},
    path::PathBuf,process::{Child,ChildStdin,Command,Stdio},sync::{Arc,Mutex,OnceLock,
    atomic::{AtomicBool,Ordering},mpsc::{self,Receiver,SyncSender}},thread::{self,JoinHandle},time::{Duration,Instant}};
use anyhow::{ensure,Context,Result};
use serde::{Deserialize,Serialize};
use windows::{core::{BOOL,PWSTR},Win32::{Foundation::{CloseHandle,COLORREF,FILETIME,HANDLE,HWND,LPARAM,WPARAM},
    Security::{GetLengthSid,GetTokenInformation,TokenUser,TOKEN_QUERY,TOKEN_USER},
    System::{SystemInformation::GetTickCount64,Threading::{GetCurrentProcess,GetProcessTimes,OpenProcess,
        OpenProcessToken,QueryFullProcessImageNameW,WaitForSingleObject,PROCESS_ACCESS_RIGHTS,
        PROCESS_NAME_WIN32,PROCESS_QUERY_LIMITED_INFORMATION}},
    UI::{Accessibility::{SetWinEventHook,UnhookWinEvent,HWINEVENTHOOK},
        Input::KeyboardAndMouse::{GetAsyncKeyState,IsWindowEnabled,VK_SHIFT},WindowsAndMessaging::*}}};

pub(super) const LEASE_MS:u64=800;
const MAX_LEASES:usize=16;
const PROPERTY:windows::core::PCWSTR=windows::core::w!("ZSDesk.ExplorerVisibilityLease.v1");
const CHANGED_STYLE:u32=WS_EX_LAYERED.0|WS_EX_TOOLWINDOW.0|WS_EX_APPWINDOW.0;

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
pub(super) struct Identity {pub window:isize,pub pid:u32,pub thread:u32,pub created:u64}
impl Identity {pub fn hwnd(&self)->HWND{HWND(self.window as*mut _)}}
#[derive(Clone,Debug,Serialize,Deserialize)]
pub(super) struct Lease {pub id:u64,pub identity:Identity,pub original_style:u32,pub started:u64,pub deadline:u64,pub foreground:bool}
impl Lease {pub fn live_at(&self,now:u64)->bool{now>=self.started&&now<self.deadline&&self.deadline-self.started<=LEASE_MS}}
#[derive(Clone,Debug,Serialize,Deserialize)]
pub(super) enum Event {Prepared(Lease),Hidden(Lease),Restored{lease:u64,reason:String},Closed{lease:u64}}
#[derive(Debug,Serialize,Deserialize)]
enum Control {Enable(bool),Heartbeat,Hide(u64),Restore(u64),Commit(u64),Shutdown}

pub(super) fn now()->u64{unsafe{GetTickCount64()}}
pub(super) fn shift_down()->bool{(unsafe{GetAsyncKeyState(VK_SHIFT.0 as i32)})<0}
struct Handle(HANDLE);
impl Drop for Handle{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}
fn process_created(process:HANDLE)->Option<u64>{
    let(mut begin,mut end,mut kernel,mut user)=(FILETIME::default(),FILETIME::default(),FILETIME::default(),FILETIME::default());
    unsafe{GetProcessTimes(process,&mut begin,&mut end,&mut kernel,&mut user)}.ok()?;
    Some((u64::from(begin.dwHighDateTime)<<32)|u64::from(begin.dwLowDateTime))
}
fn executable(process:HANDLE)->Option<PathBuf>{
    let mut text=vec![0u16;32768];let mut length=text.len()as u32;
    unsafe{QueryFullProcessImageNameW(process,PROCESS_NAME_WIN32,PWSTR(text.as_mut_ptr()),&mut length)}.ok()?;
    Some(PathBuf::from(String::from_utf16_lossy(&text[..length as usize])))
}
fn user_sid(process:HANDLE)->Option<Vec<u8>>{
    let mut token=HANDLE::default();unsafe{OpenProcessToken(process,TOKEN_QUERY,&mut token)}.ok()?;let token=Handle(token);
    let mut bytes=0u32;let _=unsafe{GetTokenInformation(token.0,TokenUser,None,0,&mut bytes)};
    if !(1..=65536).contains(&bytes){return None;}
    let mut aligned=vec![0u64;(bytes as usize+7)/8];
    unsafe{GetTokenInformation(token.0,TokenUser,Some(aligned.as_mut_ptr().cast()),bytes,&mut bytes)}.ok()?;
    let user=unsafe{&*aligned.as_ptr().cast::<TOKEN_USER>()};let length=unsafe{GetLengthSid(user.User.Sid)}as usize;
    if !(1..=1024).contains(&length){return None;}
    Some(unsafe{std::slice::from_raw_parts(user.User.Sid.0.cast::<u8>(),length)}.to_vec())
}
fn same_user(process:HANDLE)->bool{
    static SID:OnceLock<Option<Vec<u8>>>=OnceLock::new();
    let own=SID.get_or_init(||user_sid(unsafe{GetCurrentProcess()}));
    own.as_ref().zip(user_sid(process).as_ref()).is_some_and(|(a,b)|a==b)
}
fn class(window:HWND)->String{let mut text=[0u16;128];let length=unsafe{GetClassNameW(window,&mut text)}.max(0)as usize;String::from_utf16_lossy(&text[..length])}
fn root_browser(window:HWND)->bool{
    !window.0.is_null()&&unsafe{IsWindow(Some(window))}.as_bool()&&unsafe{GetAncestor(window,GA_ROOT)}==window
        &&matches!(class(window).as_str(),"CabinetWClass"|"ExploreWClass")
        &&unsafe{GetWindow(window,GW_OWNER)}.is_err()
}
pub(super) fn identity(window:HWND)->Option<Identity>{
    if !root_browser(window){return None;}
    let mut pid=0;let thread=unsafe{GetWindowThreadProcessId(window,Some(&mut pid))};if pid==0||thread==0{return None;}
    let process=Handle(unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)}.ok()?);
    let expected=PathBuf::from(std::env::var_os("SystemRoot")?).join("explorer.exe");
    if !executable(process.0).is_some_and(|path|super::same_path(&path,&expected))||!same_user(process.0){return None;}
    Some(Identity{window:window.0 as isize,pid,thread,created:process_created(process.0)?})
}
pub(super) fn matches_identity(expected:&Identity)->bool{identity(expected.hwnd()).as_ref()==Some(expected)}
fn basic_identity_matches(expected:&Identity)->bool{
    if !root_browser(expected.hwnd()){return false;}
    let mut pid=0;let thread=unsafe{GetWindowThreadProcessId(expected.hwnd(),Some(&mut pid))};
    if pid!=expected.pid||thread!=expected.thread{return false;}
    let Ok(process)=(unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)})else{return false;};let process=Handle(process);
    process_created(process.0)==Some(expected.created)
}
fn modal(window:HWND)->bool{
    !unsafe{IsWindowEnabled(window)}.as_bool()||unsafe{GetWindow(window,GW_ENABLEDPOPUP)}.is_ok_and(|popup|popup!=window&&!popup.0.is_null())
}
fn native_tab_count(window:HWND)->usize{
    unsafe extern "system" fn visit(window:HWND,value:LPARAM)->BOOL{
        let slot=match class(window).as_str(){"ShellTabWindowClass"=>Some(0),"FileExplorerTabWindowClass"=>Some(1),_=>None};
        if let Some(index)=slot{unsafe{(*(value.0 as*mut [usize;2]))[index]+=1;}}true.into()
    }
    let mut counts=[0usize;2];let _=unsafe{EnumChildWindows(Some(window),Some(visit),LPARAM((&mut counts as*mut [usize;2])as isize))};
    // Different Shell versions can nest both host classes around one tab.
    counts[0].max(counts[1])
}
fn hidden_style(original:u32)->u32{(original|WS_EX_LAYERED.0|WS_EX_TOOLWINDOW.0)&!WS_EX_APPWINDOW.0}
fn restored_style(current:u32,original:u32)->u32{(current&!CHANGED_STYLE)|(original&CHANGED_STYLE)}
fn refresh_frame(window:HWND){let _=unsafe{SetWindowPos(window,None,0,0,0,0,
    SWP_FRAMECHANGED|SWP_NOMOVE|SWP_NOSIZE|SWP_NOZORDER|SWP_NOACTIVATE|SWP_ASYNCWINDOWPOS)};}
fn marked(lease:&Lease)->bool{unsafe{GetPropW(lease.identity.hwnd(),PROPERTY)}.0 as usize==lease.id as usize}
fn hide(lease:&Lease)->bool{
    let window=lease.identity.hwnd();
    if !matches_identity(&lease.identity)||modal(window)||native_tab_count(window)>1||shift_down(){return false;}
    let current=unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32;
    // Existing layered windows may belong to an accessibility/customization
    // tool. Never replace opacity that this application does not own.
    if current&WS_EX_LAYERED.0!=0||current&CHANGED_STYLE!=lease.original_style&CHANGED_STYLE
        ||!unsafe{GetPropW(window,PROPERTY)}.0.is_null(){return false;}
    if unsafe{SetPropW(window,PROPERTY,Some(HANDLE(lease.id as usize as*mut _)))}.is_err(){return false;}
    unsafe{SetWindowLongPtrW(window,GWL_EXSTYLE,hidden_style(current)as isize);}
    if unsafe{SetLayeredWindowAttributes(window,COLORREF(0),0,LWA_ALPHA)}.is_err(){restore(lease);return false;}
    refresh_frame(window);true
}
fn restore(lease:&Lease){
    if !basic_identity_matches(&lease.identity)||!marked(lease){return;}
    let window=lease.identity.hwnd();let current=unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32;
    // Preserve unrelated style changes made by Explorer during initialization.
    if current&WS_EX_LAYERED.0!=0{let _=unsafe{SetLayeredWindowAttributes(window,COLORREF(0),255,LWA_ALPHA)};}
    unsafe{SetWindowLongPtrW(window,GWL_EXSTYLE,restored_style(current,lease.original_style)as isize);}
    let _=unsafe{RemovePropW(window,PROPERTY)};refresh_frame(window);
}

pub(super) struct Connection {
    input:Arc<Mutex<Option<ChildStdin>>>,events:Receiver<Event>,leases:Arc<Mutex<HashMap<u64,Lease>>>,
    child:Child,reader:Option<JoinHandle<()>>,
}
impl Connection {
    pub fn spawn()->Result<Self>{
        use std::os::windows::process::CommandExt;
        let parent=std::process::id();let created=process_created(unsafe{GetCurrentProcess()}).context("parent identity")?;
        let mut child=Command::new(std::env::current_exe()?).args(["--explorer-visibility-guard",&parent.to_string(),&created.to_string()])
            .creation_flags(0x08000000).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let input=Arc::new(Mutex::new(child.stdin.take()));let stdout=child.stdout.take().context("guard output")?;
        let leases=Arc::new(Mutex::new(HashMap::<u64,Lease>::new()));let recovery=leases.clone();let writer=input.clone();
        let(tx,events)=mpsc::channel();
        let reader=thread::Builder::new().name("zsdesk-explorer-lease-recovery".into()).stack_size(256*1024).spawn(move||{
            for line in BufReader::new(stdout).lines(){
                let Ok(line)=line else{break;};if line.len()>65536{break;}
                let Ok(event)=serde_json::from_str::<Event>(&line)else{break;};
                match &event {
                    Event::Prepared(lease)=>{
                        if let Ok(mut known)=recovery.lock(){known.insert(lease.id,lease.clone());}
                        // Recovery has the original style before the child is
                        // allowed to make any window invisible.
                        if send(&writer,&Control::Hide(lease.id)).is_err(){break;}
                        continue;
                    },
                    Event::Hidden(lease)=>{if let Ok(mut known)=recovery.lock(){known.insert(lease.id,lease.clone());}},
                    Event::Restored{lease,..}|Event::Closed{lease}=>{if let Ok(mut known)=recovery.lock(){known.remove(lease);}},
                }
                if tx.send(event).is_err(){break;}
            }
            // The helper can itself fail. This independent reader is never
            // blocked in Shell COM and restores every still-owned lease.
            if let Ok(mut known)=recovery.lock(){for lease in known.values(){restore(lease);}known.clear();}
        })?;
        Ok(Self{input,events,leases,child,reader:Some(reader)})
    }
    pub fn pid(&self)->u32{self.child.id()}
    pub fn enable(&self,enabled:bool)->Result<()>{send(&self.input,&Control::Enable(enabled))}
    pub fn heartbeat(&self)->Result<()>{send(&self.input,&Control::Heartbeat)}
    pub fn events(&self)->Vec<Event>{self.events.try_iter().collect()}
    pub fn active(&self,lease:&Lease)->bool{
        lease.live_at(now())&&self.leases.lock().is_ok_and(|known|known.get(&lease.id).is_some_and(|current|
            current.identity==lease.identity&&current.started==lease.started&&current.deadline==lease.deadline))
    }
    pub fn restore(&self,id:u64){let _=send(&self.input,&Control::Restore(id));}
    pub fn commit(&self,lease:&Lease)->Result<()>{ensure!(self.active(lease),"来源窗口隐藏租约已结束");send(&self.input,&Control::Commit(lease.id))}
    pub fn exited(&mut self)->bool{self.child.try_wait().ok().flatten().is_some()}
}
fn send(input:&Arc<Mutex<Option<ChildStdin>>>,command:&Control)->Result<()>{
    let mut lock=input.lock().map_err(|_|anyhow::anyhow!("guard command lock"))?;let stream=lock.as_mut().context("guard stopped")?;
    serde_json::to_writer(&mut *stream,command)?;stream.write_all(b"\n")?;stream.flush()?;Ok(())
}
impl Drop for Connection {
    fn drop(&mut self){
        let _=send(&self.input,&Control::Shutdown);if let Ok(mut input)=self.input.lock(){input.take();}
        let deadline=Instant::now()+Duration::from_secs(2);
        while self.child.try_wait().ok().flatten().is_none()&&Instant::now()<deadline{thread::sleep(Duration::from_millis(10));}
        if let Ok(mut known)=self.leases.lock(){for lease in known.values(){restore(lease);}known.clear();}
        // Only this connection's owned helper is stopped, after its windows
        // have been restored. Ordinary Explorer processes are never terminated.
        if self.child.try_wait().ok().flatten().is_none(){let _=self.child.kill();let _=self.child.wait();}
        if let Some(reader)=self.reader.take(){let _=reader.join();}
    }
}

struct Entry{lease:Lease,hidden:bool,closing:bool}
struct Watcher{enabled:bool,heartbeat:u64,known:HashSet<(isize,u32,u32)>,entries:HashMap<u64,Entry>,serial:u64,output:SyncSender<Event>}
thread_local!{static WATCHER:RefCell<Option<Watcher>>=const{RefCell::new(None)};}
impl Watcher {
    fn emit(&self,event:Event)->bool{self.output.try_send(event).is_ok()}
    fn enable(&mut self,enabled:bool){
        self.heartbeat=now();if self.enabled==enabled{return;}self.restore_all("disabled");self.known.clear();self.enabled=enabled;
        if enabled{
            unsafe extern "system" fn baseline(window:HWND,parameter:LPARAM)->BOOL{
                if root_browser(window){let mut pid=0;let tid=unsafe{GetWindowThreadProcessId(window,Some(&mut pid))};
                    unsafe{(*(parameter.0 as*mut HashSet<(isize,u32,u32)>)).insert((window.0 as isize,pid,tid));}}
                true.into()
            }
            let _=unsafe{EnumWindows(Some(baseline),LPARAM((&mut self.known as*mut HashSet<(isize,u32,u32)>)as isize))};
        }
    }
    fn observe(&mut self,event:u32,window:HWND){
        let key=window.0 as isize;
        if event==EVENT_OBJECT_DESTROY{
            self.known.retain(|(handle,_,_)|*handle!=key);
            let ids:Vec<_>=self.entries.iter().filter_map(|(id,e)|(e.lease.identity.window==key).then_some(*id)).collect();
            for id in ids{if let Some(entry)=self.entries.remove(&id){let _=self.emit(if entry.closing{Event::Closed{lease:id}}else{Event::Restored{lease:id,reason:"source-closed".into()}});}}return;
        }
        if !self.enabled||!root_browser(window){return;}
        if let Some((id,lease,hidden))=self.entries.iter().find_map(|(id,entry)|(entry.lease.identity.window==key)
            .then_some((*id,entry.lease.clone(),entry.hidden))){
            if !lease.live_at(now())||shift_down()||modal(window)||native_tab_count(window)>1{self.remove(id,"source-changed");}
            else if hidden&&event==EVENT_OBJECT_SHOW&&marked(&lease){
                let current=unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32;
                unsafe{SetWindowLongPtrW(window,GWL_EXSTYLE,hidden_style(current)as isize);}
                if unsafe{SetLayeredWindowAttributes(window,COLORREF(0),0,LWA_ALPHA)}.is_err(){self.remove(id,"visibility-lost");}
                else{refresh_frame(window);}
            }return;
        }
        if self.entries.len()>=MAX_LEASES{return;}
        let mut pid=0;let tid=unsafe{GetWindowThreadProcessId(window,Some(&mut pid))};
        if self.known.contains(&(key,pid,tid)){return;}
        self.known.insert((key,pid,tid));
        if shift_down()||modal(window)||native_tab_count(window)>1{return;}
        let Some(identity)=identity(window)else{return;};
        let original_style=unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32;
        if original_style&WS_EX_LAYERED.0!=0{return;}
        self.serial=self.serial.wrapping_add(1).max(1);
        let stamp=now();let lease=Lease{id:self.serial,identity,original_style,started:stamp,deadline:stamp.saturating_add(LEASE_MS),foreground:unsafe{GetForegroundWindow()}==window};
        if self.emit(Event::Prepared(lease.clone())){self.entries.insert(lease.id,Entry{lease,hidden:false,closing:false});}
    }
    fn command(&mut self,command:Control)->bool{
        match command {
            Control::Enable(enabled)=>self.enable(enabled),
            Control::Heartbeat=>self.heartbeat=now(),
            Control::Hide(id)=>{
                let Some(entry)=self.entries.get_mut(&id)else{return true;};
                if entry.hidden||!entry.lease.live_at(now())||!self.enabled{return true;}
                // The one and only clock start for an actual hidden window.
                entry.lease.started=now();entry.lease.deadline=entry.lease.started.saturating_add(LEASE_MS);
                if hide(&entry.lease){entry.hidden=true;let event=Event::Hidden(entry.lease.clone());
                    if !self.emit(event){self.remove(id,"event-channel-full");}}
                else{self.remove(id,"ineligible");}
            },
            Control::Restore(id)=>self.remove(id,"forwarding-cancelled"),
            Control::Commit(id)=>{
                let Some(entry)=self.entries.get_mut(&id)else{return true;};let window=entry.lease.identity.hwnd();
                // A late COM reply is insufficient: the guard must still own
                // the exact live lease when the close is dispatched.
                if !entry.hidden||entry.closing||!entry.lease.live_at(now())||!marked(&entry.lease)
                    ||!matches_identity(&entry.lease.identity)||modal(window)||native_tab_count(window)>1||shift_down(){self.remove(id,"commit-expired-or-changed");return true;}
                // Do not enqueue a posted close which could survive a restored
                // lease. A synchronous attempt consumes only its remaining
                // budget and is made after the UI confirmed the latest selection.
                let budget=entry.lease.deadline.saturating_sub(now()).min(20)as u32;
                if budget==0{self.remove(id,"close-expired");return true;}
                let accepted=unsafe{SendMessageTimeoutW(window,WM_CLOSE,WPARAM(0),LPARAM(0),SMTO_ABORTIFHUNG|SMTO_BLOCK,budget,None)}.0!=0;
                if !basic_identity_matches(&entry.lease.identity){self.entries.remove(&id);let _=self.emit(Event::Closed{lease:id});}
                else if accepted{entry.closing=true;}
                else{self.remove(id,"close-not-accepted");}
            },
            Control::Shutdown=>{self.restore_all("shutdown");return false;},
        }true
    }
    fn remove(&mut self,id:u64,reason:&str){if let Some(entry)=self.entries.remove(&id){restore(&entry.lease);let _=self.emit(Event::Restored{lease:id,reason:reason.into()});}}
    fn restore_all(&mut self,reason:&str){let ids:Vec<_>=self.entries.keys().copied().collect();for id in ids{self.remove(id,reason);}}
    fn expire(&mut self){
        let stamp=now();let ids:Vec<_>=self.entries.iter().filter_map(|(id,entry)|
            (!entry.lease.live_at(stamp)||!basic_identity_matches(&entry.lease.identity)||shift_down()
                ||modal(entry.lease.identity.hwnd())||native_tab_count(entry.lease.identity.hwnd())>1).then_some(*id)).collect();
        for id in ids{self.remove(id,"lease-expired-or-interrupted");}
        self.known.retain(|(handle,pid,tid)|{
            let window=HWND(*handle as*mut _);let mut current=0;let thread=unsafe{GetWindowThreadProcessId(window,Some(&mut current))};
            current==*pid&&thread==*tid&&unsafe{IsWindow(Some(window))}.as_bool()
        });
    }
}
unsafe extern "system" fn event_hook(_:HWINEVENTHOOK,event:u32,window:HWND,object:i32,child:i32,_:u32,_:u32){
    if object!=OBJID_WINDOW.0||child!=0{return;}
    WATCHER.with(|slot|{if let Ok(mut slot)=slot.try_borrow_mut(){if let Some(watcher)=slot.as_mut(){watcher.observe(event,window);}}});
}

pub(super) fn run_if_requested()->Option<Result<()>>{
    if std::env::args_os().nth(1).as_deref()!=Some(std::ffi::OsStr::new("--explorer-visibility-guard")){return None;}
    Some(run_guard())
}
fn run_guard()->Result<()>{
    let pid:u32=std::env::args().nth(2).context("parent pid")?.parse()?;
    let created:u64=std::env::args().nth(3).context("parent creation")?.parse()?;
    let parent=Handle(unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION|PROCESS_ACCESS_RIGHTS(0x00100000),false,pid)}?);
    ensure!(process_created(parent.0)==Some(created)&&same_user(parent.0)
        &&executable(parent.0).is_some_and(|path|std::env::current_exe().is_ok_and(|own|super::same_path(&path,&own))),"visibility guard parent mismatch");
    let(tx,commands)=mpsc::channel();let disconnected=Arc::new(AtomicBool::new(false));let input_dead=disconnected.clone();
    thread::Builder::new().name("zsdesk-explorer-guard-input".into()).stack_size(128*1024).spawn(move||{
        for line in std::io::stdin().lock().lines(){let Ok(line)=line else{break;};if line.len()>4096{break;}
            let Ok(command)=serde_json::from_str::<Control>(&line)else{break;};if tx.send(command).is_err(){break;}}
        input_dead.store(true,Ordering::Release);
    })?;
    let(output,events)=mpsc::sync_channel::<Event>(128);let output_dead=disconnected.clone();
    thread::Builder::new().name("zsdesk-explorer-guard-output".into()).stack_size(128*1024).spawn(move||{
        let mut stream=std::io::stdout().lock();for event in events{if serde_json::to_writer(&mut stream,&event).is_err()
            ||stream.write_all(b"\n").is_err()||stream.flush().is_err(){break;}}output_dead.store(true,Ordering::Release);
    })?;
    WATCHER.with(|slot|*slot.borrow_mut()=Some(Watcher{enabled:false,heartbeat:now(),known:HashSet::new(),entries:HashMap::new(),serial:1,output}));
    struct Hooks(Vec<HWINEVENTHOOK>);impl Drop for Hooks{fn drop(&mut self){for hook in self.0.drain(..){unsafe{UnhookWinEvent(hook);}}}}
    struct Recovery;impl Drop for Recovery{fn drop(&mut self){WATCHER.with(|slot|{if let Some(mut watcher)=slot.borrow_mut().take(){watcher.restore_all("guard-exit");}});}}
    let _recovery=Recovery;let mut hooks=Hooks(Vec::new());
    for event in [EVENT_SYSTEM_FOREGROUND,EVENT_OBJECT_SHOW,EVENT_OBJECT_DESTROY]{
        let hook=unsafe{SetWinEventHook(event,event,None,Some(event_hook),0,0,WINEVENT_OUTOFCONTEXT|WINEVENT_SKIPOWNPROCESS)};
        ensure!(!hook.0.is_null(),"Explorer event hook unavailable");hooks.0.push(hook);
    }
    while !disconnected.load(Ordering::Acquire)&&unsafe{WaitForSingleObject(parent.0,0)}==windows::Win32::Foundation::WAIT_TIMEOUT{
        let mut running=true;
        for command in commands.try_iter(){WATCHER.with(|slot|{if let Some(watcher)=slot.borrow_mut().as_mut(){running=watcher.command(command);}});if !running{break;}}
        if !running{break;}
        let mut message=MSG::default();while unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}
        let healthy=WATCHER.with(|slot|{if let Some(watcher)=slot.borrow_mut().as_mut(){watcher.expire();
            !watcher.enabled||now().saturating_sub(watcher.heartbeat)<2000}else{false}});
        // A live host can still have a stuck coordinator. Stop intercepting
        // new windows until that coordinator returns and establishes a new
        // guarded connection; already-hidden windows still expire at 800 ms.
        if !healthy{break;}
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn absolute_lease_deadline_cannot_be_extended_by_late_results(){
        let lease=Lease{id:1,identity:Identity{window:1,pid:1,thread:1,created:1},original_style:0,started:100,deadline:900,foreground:false};
        assert!(lease.live_at(100));assert!(lease.live_at(899));assert!(!lease.live_at(900));assert!(!lease.live_at(99));
        let invalid=Lease{deadline:901,..lease};assert!(!invalid.live_at(100));
    }
    #[test]fn restoring_visibility_preserves_unrelated_shell_style_changes(){
        let original=WS_EX_APPWINDOW.0|WS_EX_WINDOWEDGE.0;
        let hidden=hidden_style(original);assert_eq!(hidden&WS_EX_APPWINDOW.0,0);assert_ne!(hidden&WS_EX_LAYERED.0,0);
        let changed=(hidden&!WS_EX_WINDOWEDGE.0)|WS_EX_LAYOUTRTL.0;
        assert_eq!(restored_style(changed,original),WS_EX_APPWINDOW.0|WS_EX_LAYOUTRTL.0);
    }
    #[test]
    #[ignore="Hidden process-owned synthetic window; no Explorer windows or input"]
    fn native_expired_visibility_lease_restores_styles_and_rejects_late_commit()->Result<()>{
        use windows::Win32::{Foundation::HINSTANCE,System::LibraryLoader::GetModuleHandleW};
        unsafe extern "system" fn window_proc(window:HWND,message:u32,wp:WPARAM,lp:LPARAM)->windows::Win32::Foundation::LRESULT{
            unsafe{DefWindowProcW(window,message,wp,lp)}
        }
        let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);let name=windows::core::w!("CabinetWClass");
        let class=WNDCLASSW{hInstance:instance,lpszClassName:name,lpfnWndProc:Some(window_proc),..Default::default()};
        ensure!(unsafe{RegisterClassW(&class)}!=0,"synthetic class registration");
        struct Class(HINSTANCE);impl Drop for Class{fn drop(&mut self){let _=unsafe{UnregisterClassW(windows::core::w!("CabinetWClass"),Some(self.0))};}}
        let _class=Class(instance);
        let window=unsafe{CreateWindowExW(WS_EX_APPWINDOW,name,windows::core::w!("visibility recovery fixture"),WS_OVERLAPPEDWINDOW,
            -30000,-30000,320,200,None,None,Some(instance),None)}?;
        struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
        let _window=Window(window);ensure!(!unsafe{IsWindowVisible(window)}.as_bool(),"test must remain hidden");
        let mut pid=0;let thread=unsafe{GetWindowThreadProcessId(window,Some(&mut pid))};ensure!(pid==std::process::id(),"test owns only its process window");
        let original=unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32;
        let stamp=now();let lease=Lease{id:42,identity:Identity{window:window.0 as isize,pid,thread,
            created:process_created(unsafe{GetCurrentProcess()}).context("test process time")?},original_style:original,
            started:stamp.saturating_sub(LEASE_MS),deadline:stamp,foreground:false};
        // Install the lease only on this synthetic window. Production capture
        // still requires the signed-in user's real system Explorer executable.
        unsafe{SetPropW(window,PROPERTY,Some(HANDLE(lease.id as usize as*mut _)))?;
            SetWindowLongPtrW(window,GWL_EXSTYLE,hidden_style(original)as isize);
            SetLayeredWindowAttributes(window,COLORREF(0),0,LWA_ALPHA)?;}
        let(tx,rx)=mpsc::sync_channel(8);let mut watcher=Watcher{enabled:true,heartbeat:now(),known:HashSet::new(),
            entries:HashMap::from([(lease.id,Entry{lease:lease.clone(),hidden:true,closing:false})]),serial:42,output:tx};
        watcher.expire();assert!(watcher.entries.is_empty());assert!(matches!(rx.try_recv(),Ok(Event::Restored{lease:42,..})));
        assert_eq!(unsafe{GetWindowLongPtrW(window,GWL_EXSTYLE)}as u32&CHANGED_STYLE,original&CHANGED_STYLE);
        assert!(!marked(&lease));assert!(watcher.command(Control::Commit(42)));
        assert!(unsafe{IsWindow(Some(window))}.as_bool(),"late commit closed a recovered window");
        assert!(!unsafe{IsWindowVisible(window)}.as_bool(),"synthetic test window became visible");
        Ok(())
    }
}
