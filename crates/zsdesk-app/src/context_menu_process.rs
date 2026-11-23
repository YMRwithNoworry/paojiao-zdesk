//! Bounded process isolation for Shell extensions. Each request owns a fresh
//! immutable selection/menu; an idle STA may serve a later verified request.
use std::{fs,io::Read,path::{Path,PathBuf},sync::{Mutex,atomic::{AtomicU64,Ordering}},time::{Duration,Instant}};
#[cfg(test)]use std::thread;
use std::os::windows::{fs::MetadataExt,io::AsRawHandle};
use anyhow::{Context,Result,ensure};
use serde::{Deserialize,Serialize};
use windows::{core::{PCWSTR,w},Win32::{Foundation::{HWND,HINSTANCE,LPARAM,LRESULT,WPARAM,POINT,HANDLE,CloseHandle,FILETIME},
    System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize},Threading::{OpenProcess,GetProcessTimes,PROCESS_QUERY_LIMITED_INFORMATION},DataExchange::GetClipboardOwner,JobObjects::*},
    UI::WindowsAndMessaging::*}};
use zsdesk_windows::shell::menu::{ContextMenuOptions,MenuInterceptions,MenuOutcome,ShellMenu,forward_menu_message};
use zsdesk_core::model::explorer::ShellViewState;
#[path="context_menu_background.rs"]mod background_view;
#[path="context_menu_child.rs"]mod child;
#[path="context_menu_common.rs"]mod common;
#[path="context_menu_prewarm.rs"]pub(crate) mod prewarm;
use child::OwnedChild as Child;

const MAX_REQUEST:u64=16*1024*1024;
const MAX_STATUS:u64=2*1024*1024;
const MAX_ITEMS:usize=8192;
const WORKER_MEMORY_LIMIT:usize=512*1024*1024;
static SERIAL:AtomicU64=AtomicU64::new(1);
static RETIRING:Mutex<Vec<RetiredChild>>=Mutex::new(Vec::new());
const IDLE_TIMEOUT:Duration=Duration::from_secs(30);
const MAX_SESSION_REQUESTS:u32=32;
const MAX_IDLE_WORKERS:usize=1;
static IDLE:Mutex<Vec<IdleWorker>>=Mutex::new(Vec::new());
static DIAGNOSTICS_LOCK:Mutex<()>=Mutex::new(());
static PREPARING:std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
static REAPING:std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
struct Origin {thread:u32,desktop:usize,started:Instant,counter:u64,layout:Option<ShellViewState>,view_context:Option<MenuViewContext>,#[cfg(test)]probe:Option<String>}
impl Origin {fn capture()->Self{let thread=unsafe{windows::Win32::System::Threading::GetCurrentThreadId()};Self{
    thread,desktop:unsafe{GetThreadDesktop(thread)}as usize,started:Instant::now(),counter:monotonic_us(),layout:None,view_context:None,
    #[cfg(test)]probe:std::env::var("ZSDESK_MENU_TEST_PROBE").ok()}}}

struct EventSet { request:usize,status:usize,decision:usize,cancel:usize,dismiss:usize }
impl EventSet {
    fn names(session:&str)->[String;5] { ["request","status","decision","cancel","dismiss"].map(|part|format!("Local\\ZSDesk.Menu.{session}.{part}")) }
    fn create(session:&str)->Result<Self> {
        use windows::Win32::System::Threading::CreateEventW;
        let mut handles=Vec::new();
        for (index,name) in Self::names(session).iter().enumerate() {
            match unsafe{CreateEventW(None,index>=3,false,PCWSTR(wide(name).as_ptr()))} {
                Ok(handle)=>handles.push(handle.0 as usize),
                Err(error)=>{for handle in handles{let _=unsafe{CloseHandle(HANDLE(handle as*mut _))};}return Err(error.into());}
            }
        }
        Ok(Self{request:handles[0],status:handles[1],decision:handles[2],cancel:handles[3],dismiss:handles[4]})
    }
    fn open(session:&str)->Result<Self> {
        use windows::Win32::System::Threading::{OpenEventW,SYNCHRONIZATION_ACCESS_RIGHTS};
        let mut handles=Vec::new();
        for name in Self::names(session) {
            match unsafe{OpenEventW(SYNCHRONIZATION_ACCESS_RIGHTS(0x00100002),false,PCWSTR(wide(&name).as_ptr()))} {
                Ok(handle)=>handles.push(handle.0 as usize),
                Err(error)=>{for handle in handles{let _=unsafe{CloseHandle(HANDLE(handle as*mut _))};}return Err(error.into());}
            }
        }
        Ok(Self{request:handles[0],status:handles[1],decision:handles[2],cancel:handles[3],dismiss:handles[4]})
    }
    fn signal(handle:usize){let _=unsafe{windows::Win32::System::Threading::SetEvent(HANDLE(handle as*mut _))};}
    fn signaled(handle:usize)->bool{unsafe{windows::Win32::System::Threading::WaitForSingleObject(HANDLE(handle as*mut _),0)}.0==0}
    fn reset(&self){for handle in [self.request,self.status,self.decision,self.cancel,self.dismiss]{let _=unsafe{windows::Win32::System::Threading::ResetEvent(HANDLE(handle as*mut _))};}}
}
impl Drop for EventSet{fn drop(&mut self){for handle in [self.request,self.status,self.decision,self.cancel,self.dismiss]{let _=unsafe{CloseHandle(HANDLE(handle as*mut _))};}}}

#[link(name="user32")]unsafe extern "system"{
    #[link_name="MsgWaitForMultipleObjectsEx"]
    fn wait_messages(count:u32,handles:*const HANDLE,milliseconds:u32,wake:u32,flags:u32)->u32;
}
pub(crate) fn wait_signal(handles:&[usize],milliseconds:u32)->Option<usize>{
    let handles:Vec<_>=handles.iter().map(|raw|HANDLE(*raw as*mut _)).collect();
    let result=unsafe{wait_messages(handles.len()as u32,handles.as_ptr(),milliseconds,QS_ALLINPUT.0,MWMO_INPUTAVAILABLE.0)};
    (result<handles.len()as u32).then_some(result as usize)
}
fn monotonic_us()->u64{
    static FREQUENCY:std::sync::OnceLock<i64>=std::sync::OnceLock::new();
    #[link(name="kernel32")]unsafe extern "system"{
        #[link_name="QueryPerformanceCounter"]fn counter(value:*mut i64)->i32;
        #[link_name="QueryPerformanceFrequency"]fn frequency(value:*mut i64)->i32;
    }
    let frequency=*FREQUENCY.get_or_init(||{let mut value=0;let _=unsafe{frequency(&mut value)};value});
    let mut value=0;
    if frequency>0&&unsafe{counter(&mut value)}!=0{((value as u128)*1_000_000/(frequency as u128)).min(u64::MAX as u128)as u64}
    else{unsafe{windows::Win32::System::SystemInformation::GetTickCount64()}.saturating_mul(1000)}
}
fn micros(duration:Duration)->u64{duration.as_micros().min(u64::MAX as u128)as u64}

#[derive(Clone,Copy)]
struct MenuOwner { window:isize,thread:u32,process:u32 }
impl MenuOwner {
    fn capture(window:HWND)->Self{
        let mut process=0;
        let thread=unsafe{GetWindowThreadProcessId(window,Some(&mut process))};
        Self{window:window.0 as isize,thread,process}
    }
    fn alive(self)->bool{
        let live=Self::capture(HWND(self.window as *mut _));
        self.thread!=0 && live.thread==self.thread && live.process==self.process
    }
}
struct IdleWorker {
    child:Option<Child>,job:Option<usize>,workspace:PathBuf,session:String,token:String,events:Option<EventSet>,
    count:u32,created:u64,last_used:Instant,desktop:usize,thread:u32,
    /// A command already ran in this process. Its dialogs, clipboard data or
    /// file operations own the process lifetime: it exits by itself and is
    /// never terminated on retirement.
    committed:bool,
    native_owner:Option<MenuOwner>,
    prepared_drawn:bool,
}
impl IdleWorker {
    fn within_lifetime(&self,margin:Duration)->bool{
        match self.native_owner{Some(owner)=>owner.alive(),None=>self.last_used.elapsed()<IDLE_TIMEOUT.saturating_sub(margin)}
    }
}
impl Drop for IdleWorker {fn drop(&mut self){
    let Some(child)=self.child.take()else{return;};
    if let Some(events)=&self.events{EventSet::signal(events.cancel);EventSet::signal(events.request);}
    retire_child(child,self.job.take(),self.workspace.clone(),self.created,self.committed);
}}
struct RetiredChild{child:Child,job:Option<usize>,workspace:PathBuf,identity:u64,committed:bool,deadline:Instant,kill_requested:bool}
impl Drop for RetiredChild{fn drop(&mut self){if let Some(job)=self.job.take(){let _=unsafe{CloseHandle(HANDLE(job as*mut _))};}}}
fn retire_child(child:Child,job:Option<usize>,workspace:PathBuf,identity:u64,committed:bool){
    RETIRING.lock().unwrap_or_else(|error|error.into_inner()).push(RetiredChild{child,job,workspace,identity,committed,
        deadline:Instant::now()+Duration::from_secs(2),kill_requested:false});
    start_reaper();
}
fn start_reaper(){
    if REAPING.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err(){return;}
    if std::thread::Builder::new().name("zsdesk-menu-retirement".into()).spawn(||{
        loop{
            let pending=collect_workers();let idle=!IDLE.lock().unwrap_or_else(|e|e.into_inner()).is_empty();
            if pending==0&&!idle{break;}
            std::thread::sleep(Duration::from_millis(if pending==0{1000}else{100}));
        }
        REAPING.store(false,Ordering::Release);
        if !IDLE.lock().unwrap_or_else(|e|e.into_inner()).is_empty()||!RETIRING.lock().unwrap_or_else(|e|e.into_inner()).is_empty(){start_reaper();}
    }).is_err(){REAPING.store(false,Ordering::Release);}
}
pub fn shutdown_idle_worker(){prewarm::interrupt();let idle=std::mem::take(&mut*IDLE.lock().unwrap_or_else(|error|error.into_inner()));drop(idle);}
pub fn shutdown_idle_worker_for_thread(){
    let thread=unsafe{windows::Win32::System::Threading::GetCurrentThreadId()};
    let idle={let mut pool=IDLE.lock().unwrap_or_else(|error|error.into_inner());pool.iter().position(|idle|idle.thread==thread).map(|index|pool.swap_remove(index))};drop(idle);
}
pub fn idle_worker_diagnostics()->serde_json::Value{
    let idle=IDLE.lock().unwrap_or_else(|error|error.into_inner());
    serde_json::Value::Array(idle.iter().map(|idle|serde_json::json!({"pid":idle.child.as_ref().map(Child::id),"created":idle.created,
        "identity_verified":idle.child.as_ref().is_some_and(|child|created(child.id())==Some(idle.created)),
        "owner_thread":idle.thread,"idle_milliseconds":micros(idle.last_used.elapsed())/1000,
        "owner_window_lifetime":idle.native_owner.is_some(),"idle_limit_milliseconds":idle.native_owner.is_none().then_some(IDLE_TIMEOUT.as_millis()as u64),"requests":idle.count,"request_limit":MAX_SESSION_REQUESTS})).collect())
}
fn retire_expired_idle(){
    let retired={let mut idle=IDLE.lock().unwrap_or_else(|error|error.into_inner());let mut retired=Vec::new();let mut index=0;
        while index<idle.len(){let expired=!idle[index].within_lifetime(Duration::ZERO)||idle[index].child.as_mut().is_none_or(|child|child.try_wait().ok().flatten().is_some());
            if expired{retired.push(idle.swap_remove(index));}else{index+=1;}}retired};drop(retired);
}
pub fn pending_workers()->usize{
    let retiring=collect_workers();
    // A parked worker needs no host-loop polling. Still drain the reaper on
    // shutdown: it may be finishing workspace cleanup after removing the child.
    let draining=REAPING.load(Ordering::Acquire)&&IDLE.lock().unwrap_or_else(|e|e.into_inner()).is_empty();
    retiring+PREPARING.load(Ordering::Acquire)+usize::from(draining)+usize::from(prewarm::running())
}
fn collect_workers()->usize{
    retire_expired_idle();let mut finished=Vec::new();
    let count={let mut workers=RETIRING.lock().unwrap_or_else(|error|error.into_inner());let mut index=0;
        while index<workers.len(){
            let worker=&mut workers[index];
            if let Some(exit)=worker.child.try_wait().ok().flatten(){
                if !exit.success()&&!worker.kill_requested{retain_retired_fault(worker,exit.code());}
                finished.push(workers.swap_remove(index));continue;
            }
            if !worker.committed&&!worker.kill_requested&&Instant::now()>=worker.deadline
                &&created(worker.child.id())==Some(worker.identity)&&!process_visible_windows(worker.child.id(),HWND::default()){
                worker.kill_requested=worker.child.kill().is_ok();
            }
            index+=1;
        }workers.len()};
    for worker in finished{cleanup(&worker.workspace);}count
}
pub fn diagnostics_directory()->PathBuf{
    std::env::var_os("ZSDESK_MENU_ROOT").map(PathBuf::from).unwrap_or_else(||dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join(crate::edition::profile_name()).join("menus"))
}
pub fn read_recent_diagnostics()->serde_json::Value{
    let root=diagnostics_directory();serde_json::json!({"current":read_small::<serde_json::Value>(&root.join("menu-diagnostics.current.json"),128*1024).ok(),
    "previous":read_small::<serde_json::Value>(&root.join("menu-diagnostics.previous.json"),128*1024).ok(),
    "last_crash":read_small::<serde_json::Value>(&root.join("menu-last-crash.json"),128*1024).ok(),"idle_workers":idle_worker_diagnostics()})
}
// A subsequent successful right-click must not overwrite the latest crash.
fn save_crash(root:&Path,exit:Option<i32>,value:&serde_json::Value)->bool{
    exit.is_some_and(|code|code!=0)&&write(&root.join("menu-last-crash.json"),value).is_ok()
}
fn retain_retired_fault(worker:&RetiredChild,exit:Option<i32>){
    let _guard=DIAGNOSTICS_LOCK.lock().unwrap_or_else(|error|error.into_inner());
    let Some(root)=worker.workspace.parent()else{return;};let current=root.join("menu-diagnostics.current.json");
    let old=read_small::<serde_json::Value>(&current,128*1024).ok();
    if old.as_ref().is_some_and(|old|old["worker_pid"].as_u64()==Some(u64::from(worker.child.id()))&&old["worker_exit_code"].as_i64()==exit.map(i64::from)){return;}
    let status=read_small::<Status>(&worker.workspace.join("status.json"),MAX_STATUS).ok();
    let value=serde_json::json!({"schema":"ZSDesk.MenuDiagnostics.v1","timestamp_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|time|time.as_millis()as u64),
        "parent_pid":std::process::id(),"worker_pid":worker.child.id(),"worker_created":worker.identity,
        "phase":status.as_ref().map(|status|status.phase.as_str()).unwrap_or("retiring"),"worker_exit_code":exit,"worker_exit_hex":exit.map(|code|format!("0x{:08X}",code as u32)),
        "committed":worker.committed,"timing":status.as_ref().map(|status|&status.timing),"native_menu":status.as_ref().map(|status|&status.native_menu),"fault":read_small::<serde_json::Value>(&worker.workspace.join("fault.json"),16384).ok(),
        "error":"系统扩展菜单进程在请求完成后异常退出","stderr":redacted_error(&worker_stderr(&worker.workspace))});
    save_crash(root,exit,&value);
    if let Some(old)=old{let _=write(&root.join("menu-diagnostics.previous.json"),&old);}let _=write(&current,&value);
}
fn redacted_error(value:&str)->String{
    value.lines().take(24).map(|line|{
        let bytes=line.as_bytes();let start=(0..bytes.len()).find(|&index|
            (index+2<bytes.len()&&bytes[index].is_ascii_alphabetic()&&bytes[index+1]==b':'&&matches!(bytes[index+2],b'\\'|b'/'))
            ||(index+1<bytes.len()&&bytes[index]==b'\\'&&bytes[index+1]==b'\\'));
        match start{Some(index)=>format!("{}<路径已省略>",&line[..index]),None=>line.to_owned()}
    }).collect::<Vec<_>>().join("\n").chars().take(4096).collect()
}
#[link(name="kernel32")]unsafe extern "system"{
    fn SetErrorMode(mode:u32)->u32;
    #[cfg(test)]fn RaiseException(code:u32,flags:u32,count:u32,arguments:*const usize);
}
#[link(name="user32")]unsafe extern "system"{fn GetThreadDesktop(thread:u32)->*mut core::ffi::c_void;}
#[derive(Clone,Copy,Debug,Serialize,Deserialize,PartialEq,Eq)]
pub enum Mode{More,Properties,Open,Paste,PasteBackground,Background,New,CatalogSelection,CatalogBackground,PrepareSelection,PrepareBackground}
impl Mode {
    fn prewarm(self)->bool { matches!(self,Self::PrepareSelection|Self::PrepareBackground) }
}
#[derive(Clone,Debug,Default,Serialize,Deserialize)]
pub struct MenuViewContext{
    pub folder:Option<Vec<u8>>,
    pub results:bool,
    pub keyboard:bool,
    pub f10:bool,
}
#[derive(Clone,Serialize,Deserialize)]
struct Request{
    token:String,parent:u32,parent_created:u64,owner:isize,point:[i32;2],pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,
    #[serde(default)]session:String,
    #[serde(default)]ordinal:u32,
    #[serde(default)]english:Option<bool>,
    options:ContextMenuOptions,rename:bool,open:bool,mode:Mode,extended:bool,
    #[serde(default)]file_actions:bool,
    #[serde(default)]layout:Option<ShellViewState>,
    #[serde(default)]view_context:Option<MenuViewContext>,
    #[cfg(test)]probe:Option<String>,
}
#[derive(Clone,Debug,Default,Serialize,Deserialize)]
pub struct MenuTimings {
    #[serde(default)]pub background_view_reused:bool,
    pub worker_thread:u32,
    pub fault_report_available:bool,
    pub reused_process:bool,pub request_write_microseconds:u64,pub process_spawn_microseconds:u64,
    pub com_microseconds:u64,pub worker_setup_microseconds:u64,pub validation_microseconds:u64,
    pub worker_to_menu_microseconds:Option<u64>,pub parent_to_menu_microseconds:Option<u64>,pub status_reads:u64,
    pub parent_validation_microseconds:u64,pub worker_validation_count:u64,pub menu_open_observation:Option<String>,
    #[serde(default)]pub foreground_microseconds:u64,
    #[serde(default)]pub tracking_to_menu_microseconds:Option<u64>,
}
#[derive(Clone,Serialize,Deserialize)]
struct Status{token:String,pid:u32,window:isize,phase:String,validation:u64,before_invoke:bool,outcome:Option<MenuOutcome>,error:Option<String>,
    #[serde(default)]error_hresult:Option<i32>,#[serde(default)]error_phase:Option<String>,bind_microseconds:u64,populate_microseconds:u64,
    #[serde(default)]catalog:Vec<zsdesk_windows::shell::menu::MenuCatalogItem>,
    #[serde(default)]created:Vec<zsdesk_windows::shell::new_menu::CreatedItem>,
    #[serde(default)]layout:Option<ShellViewState>,
    #[serde(default)]layout_sort_changed:bool,
    #[serde(default)]reusable:bool,
    #[serde(default)]timing:MenuTimings,
    #[serde(default)]native_menu:serde_json::Value,
    #[cfg(test)]#[serde(default)]probe_menu_open:bool,
    #[cfg(test)]#[serde(default)]probe_classic_loop:bool,
    #[cfg(test)]#[serde(default)]probe_expanded_requested:bool,
    #[cfg(test)]#[serde(default)]probe_command_messages:u64,
    #[cfg(test)]#[serde(default)]probe_clipboard_after_invoke:serde_json::Value,
}
#[derive(Serialize,Deserialize)]
struct Decision{token:String,validation:u64,permit:bool}
#[derive(Debug)]
struct MenuFailure{phase:String,code:Option<i32>,detail:String,invoked:bool}
impl std::fmt::Display for MenuFailure{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{write!(f,"{}",self.detail)}}
impl std::error::Error for MenuFailure{}
fn worker_failure(status:&Status)->anyhow::Error{MenuFailure{
    phase:status.error_phase.clone().unwrap_or_else(||status.phase.clone()),code:status.error_hresult,
    detail:status.error.clone().unwrap_or_else(||"系统菜单未返回结果".into()),invoked:status.before_invoke,
}.into()}
pub fn failure_code(error:&anyhow::Error)->windows::core::HRESULT{
    windows::core::HRESULT(error.downcast_ref::<MenuFailure>().and_then(|error|error.code)
        .or_else(||error.chain().find_map(|cause|cause.downcast_ref::<windows::core::Error>().map(|error|error.code().0)))
        .or_else(||error.chain().find_map(|cause|cause.downcast_ref::<std::io::Error>().and_then(|error|{
            match error.raw_os_error(){Some(code)if code<0=>Some(code),Some(code)if code>0=>Some(windows::core::HRESULT::from_win32(code as u32).0),
                _ if error.kind()==std::io::ErrorKind::PermissionDenied=>Some(0x80070005u32 as i32),_=>None}
        })))
        .unwrap_or(0x80004005u32 as i32))
}
pub fn failure_message(error:&anyhow::Error)->String{
    if let Some(failure)=error.downcast_ref::<MenuFailure>(){
        let action=if failure.invoked{"右键菜单命令执行失败"}else{match failure.phase.as_str(){
            "binding"=>"无法获取当前位置的系统菜单","populating"=>"系统菜单项目加载失败","tracking"=>"系统菜单显示失败",_=>"系统菜单未完成",
        }};
        return format!("{action}：{}",failure.detail);
    }
    format!("系统菜单未完成：{error:#}")
}
fn wide(value:&str)->Vec<u16>{value.encode_utf16().chain(Some(0)).collect()}
fn created(pid:u32)->Option<u64>{
    let process=unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)}.ok()?;
    let(mut begin,mut end,mut kernel,mut user)=(FILETIME::default(),FILETIME::default(),FILETIME::default(),FILETIME::default());
    let result=unsafe{GetProcessTimes(process,&mut begin,&mut end,&mut kernel,&mut user)};let _=unsafe{CloseHandle(process)};
    result.ok().map(|_|u64::from(begin.dwHighDateTime)<<32|u64::from(begin.dwLowDateTime))
}
fn parent_alive(request:&Request)->bool{
    let mut pid=0;let hwnd=HWND(request.owner as*mut _);unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
    pid==request.parent&&created(pid)==Some(request.parent_created)&&unsafe{IsWindow(Some(hwnd))}.as_bool()
}
fn write(path:&Path,value:&impl Serialize)->Result<()>{
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING};
    // Transient IPC needs an atomic publication, not a durable disk flush.
    // MOVEFILE_WRITE_THROUGH on every validation/status packet serialized the
    // menu-open path behind storage/AV latency without adding protocol safety.
    let temporary=path.with_extension(format!("{}.tmp",std::process::id()));
    fs::write(&temporary,serde_json::to_vec(value)?)?;
    let from:Vec<_>=temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to:Vec<_>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    // A concurrent Shell/AV reader can briefly deny replacement on Windows.
    // Retry the same unpublished bytes off the UI thread; never publish a
    // partial JSON packet or turn a persistent failure into a successful menu.
    let deadline=Instant::now()+Duration::from_millis(250);
    loop {match unsafe{MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_REPLACE_EXISTING)} {
        Ok(())=>return Ok(()),
        Err(error) if matches!(error.code().0 as u32,0x80070005|0x80070020|0x80070021)&&Instant::now()<deadline=>std::thread::sleep(Duration::from_millis(5)),
        Err(error)=>{let _=fs::remove_file(&temporary);return Err(error).context("publish menu protocol packet");},
    }}
}
fn read_small<T:serde::de::DeserializeOwned>(path:&Path,limit:u64)->Result<T>{
    let metadata=fs::symlink_metadata(path)?;ensure!(metadata.is_file()&&metadata.file_attributes()&0x400==0&&metadata.len()<=limit,"菜单协议文件无效");
    let mut bytes=Vec::new();fs::File::open(path)?.take(limit+1).read_to_end(&mut bytes)?;ensure!(bytes.len()as u64<=limit,"菜单协议文件过大");Ok(serde_json::from_slice(&bytes)?)
}
fn worker_stderr(workspace:&Path)->String{
    use std::io::{Seek,SeekFrom};
    let path=workspace.join("worker.stderr.log");
    let Ok(metadata)=fs::symlink_metadata(&path)else{return String::new();};
    if !metadata.is_file()||metadata.file_attributes()&0x400!=0{return String::new();}
    let mut bytes=Vec::new();let mut skipped=false;
    let read=(||->std::io::Result<()>{
        let mut file=fs::File::open(path)?;let start=file.metadata()?.len().saturating_sub(4096);skipped=start!=0;
        file.seek(SeekFrom::Start(start))?;file.take(4096).read_to_end(&mut bytes)?;Ok(())
    })();
    if read.is_err(){return String::new();}
    // A reused worker keeps appending. Preserve its latest complete lines, and
    // discard a cut leading line so a truncated path cannot evade redaction.
    let bytes=if skipped{let Some(end)=bytes.iter().position(|b|*b==b'\n')else{return String::new();};&bytes[end+1..]}else{&bytes[..]};
    let text=String::from_utf8_lossy(bytes);let mut lines=text.lines().rev().take(24).collect::<Vec<_>>();lines.reverse();
    lines.join("\n").trim().to_owned()
}
fn valid_pidl(bytes:&[u8])->bool{
    if bytes.len()<2||bytes.len()>65536{return false;}
    let mut offset=0usize;
    while offset+2<=bytes.len(){let size=u16::from_le_bytes([bytes[offset],bytes[offset+1]])as usize;
        if size==0{return offset+2==bytes.len();}if size<2{return false;}let Some(next)=offset.checked_add(size)else{return false;};if next>bytes.len(){return false;}offset=next;
    }false
}
fn validate(request:&Request)->Result<()>{
    ensure!(!request.session.is_empty()&&request.session.len()<128&&request.session.bytes().all(|c|c.is_ascii_alphanumeric()||c==b'-'),"菜单会话标识无效");
    ensure!((1..=MAX_SESSION_REQUESTS).contains(&request.ordinal),"菜单会话请求数量无效");
    ensure!(!request.token.is_empty()&&request.token.len()<128,"菜单请求标识无效");
    ensure!(request.pidls.len()<=MAX_ITEMS&&request.pidls.iter().all(|pidl|valid_pidl(pidl)),"菜单选择数据无效或过多");
    ensure!(request.pidls.iter().map(Vec::len).sum::<usize>()<=3*1024*1024,"菜单选择标识总量过大");
    ensure!(!request.pidls.is_empty()||request.folder.is_some(),"没有可用的系统菜单目标");
    if request.mode.prewarm(){
        ensure!(request.pidls.len()==1 &&
            !request.options.quick_first && !request.extended,"无效的原生预热请求");
    }
    ensure!(!matches!(request.mode,Mode::Background|Mode::New|Mode::CatalogBackground|Mode::PasteBackground)||request.pidls.len()==1,"背景菜单需要一个文件夹标识");
    ensure!(request.folder.as_ref().is_none_or(|folder|folder.is_absolute()),"菜单目录必须是绝对路径");
    if let Some(context)=&request.view_context{ensure!(context.folder.as_ref().is_none_or(|pidl|valid_pidl(pidl)),"菜单源文件夹标识无效");}
    for keys in [&request.options.hidden_verbs,&request.options.shown_verbs]{ensure!(keys.len()<=1024&&keys.iter().all(|verb|verb.len()<=512&&!verb.contains('\0')),"菜单设置无效");}
    if let Some(layout)=&request.layout {
        let allows_columns = matches!(request.mode, Mode::Background | Mode::New | Mode::CatalogBackground);
        ensure!(matches!(request.mode, Mode::More | Mode::CatalogSelection) || allows_columns, "背景视图设置无效");
        ensure!((allows_columns || layout.columns.is_empty()) && zsdesk_windows::shell::valid_menu_layout(layout), "背景视图设置无效");
    }
    Ok(())
}
fn cleanup(workspace:&Path){
    if workspace.file_name().is_none_or(|name|!name.to_string_lossy().starts_with("menu-")){return;}
    for name in ["request.json","status.json","opened.json","decision.json","fault.json","cancel","start","worker.stderr.log"]{
        let path=workspace.join(name);if fs::symlink_metadata(&path).is_ok_and(|metadata|metadata.is_file()&&metadata.file_attributes()&0x400==0){let _=fs::remove_file(path);}
    }
    if let Ok(entries)=fs::read_dir(workspace){for entry in entries.flatten(){
        let name=entry.file_name();let name=name.to_string_lossy();let parts:Vec<_>=name.split('.').collect();
        if parts.len()==3&&["request","status","opened","decision"].contains(&parts[0])&&parts[1].bytes().all(|c|c.is_ascii_digit())&&parts[2]=="tmp"
            &&entry.path().symlink_metadata().is_ok_and(|metadata|metadata.is_file()&&metadata.file_attributes()&0x400==0){let _=fs::remove_file(entry.path());}
    }}
    let _=fs::remove_dir(workspace);
}

#[derive(Serialize,Deserialize)]
struct MenuOpened{token:String,pid:u32,at:u64,worker_started:u64,observation:String,#[serde(default)]tracking_started:u64}

pub struct Client{
    child:Option<Child>,job:Option<usize>,workspace:PathBuf,token:String,session:String,events:Option<EventSet>,
    child_created:u64,ordinal:u32,thread:u32,desktop:usize,started:Instant,started_counter:u64,
    /// The reused process already executed a command for an earlier request.
    inherited_commit:bool,
    last_validation:u64,committed:bool,complete:bool,status_dirty:bool,last_status:Option<Status>,timing:MenuTimings,
    mode:Mode,item_count:usize,owner_class:String,owner:MenuOwner,diagnostic_written:bool,
}
impl Client{
    pub fn spawn(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode)->Result<Self>{
        Self::spawn_extended(owner,point,pidls,folder,options,intercept,mode,false)
    }
    pub fn spawn_extended(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool)->Result<Self>{
        Self::spawn_at(owner,point,pidls,folder,options,intercept,mode,extended,Origin::capture())
    }
    pub fn spawn_with_layout(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool,layout:Option<ShellViewState>)->Result<Self>{
        let mut origin=Origin::capture();origin.layout=layout;
        Self::spawn_at(owner,point,pidls,folder,options,intercept,mode,extended,origin)
    }
    pub fn spawn_with_context(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool,layout:Option<ShellViewState>,view_context:MenuViewContext)->Result<Self>{
        let mut origin=Origin::capture();origin.layout=layout;origin.view_context=Some(view_context);
        Self::spawn_at(owner,point,pidls,folder,options,intercept,mode,extended,origin)
    }
    fn spawn_at(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool,origin:Origin)->Result<Self>{
        // Keep worker selection and its lifetime policy on the effective OS presentation.
        let options=zsdesk_windows::shell::menu::resolve_presentation(options);
        if !mode.prewarm(){prewarm::interrupt();}
        let started=origin.started;let started_counter=origin.counter;
        let root=diagnostics_directory();ensure!(root.is_absolute(),"菜单缓存目录必须是绝对路径");fs::create_dir_all(&root)?;
        let parent_created=created(std::process::id()).context("无法核对菜单父进程")?;
        retire_expired_idle();
        let thread=origin.thread;let desktop=origin.desktop;
        let token=format!("{}-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
        let mut reusable=None;
        for _ in 0..MAX_IDLE_WORKERS{
            let cached={let mut pool=IDLE.lock().unwrap_or_else(|error|error.into_inner());
                let compatible=|idle:&IdleWorker|idle.desktop==desktop&&idle.workspace.parent()==Some(root.as_path())&&idle.count<MAX_SESSION_REQUESTS
                    &&idle.within_lifetime(Duration::from_secs(2));
                // The child retains its own STA; changing the parent window
                // sends a fresh serialized request, never a COM interface.
                let style_matches=|idle:&IdleWorker| compatible(idle) && idle.prepared_drawn == options.drawn;
                let index=pool.iter().position(|idle|idle.thread==thread && style_matches(idle))
                    .or_else(||pool.iter().position(style_matches))
                    .or_else(||pool.iter().position(compatible));
                index.map(|index|pool.swap_remove(index))};
            let Some(idle)=cached else{break;};
            // Switching styles must not lend a parked native STA (and any
            // resources it already owns) to an on-demand drawn session.
            if idle.prepared_drawn != options.drawn {drop(idle);break;}
            let valid=idle.child.as_ref().is_some_and(|child|created(child.id())==Some(idle.created))
                &&read_small::<Status>(&idle.workspace.join("status.json"),MAX_STATUS).is_ok_and(|status|
                    status.phase=="idle"&&status.reusable&&status.token==idle.token&&idle.child.as_ref().is_some_and(|child|status.pid==child.id()));
            if valid{reusable=Some(idle);break;}
            // Retirement keeps the existing committed-operation lifetime rule.
            drop(idle);
        }
        let (mut child,mut job,workspace,session,events,ordinal,child_created,reused,inherited_commit)=if let Some(mut idle)=reusable{
            (idle.child.take(),idle.job.take(),idle.workspace.clone(),idle.session.clone(),idle.events.take().unwrap(),idle.count+1,idle.created,true,idle.committed)
        }else{
            let workspace=root.join(format!("menu-{token}"));fs::create_dir(&workspace)?;
            let events=match EventSet::create(&token){Ok(value)=>value,Err(error)=>{cleanup(&workspace);return Err(error);}};
            (None,None,workspace,token.clone(),events,1,0,false,false)
        };
        events.reset();
        let item_count=pidls.len();
        let mut owner_name=[0u16;128];let owner_length=unsafe{GetClassNameW(owner,&mut owner_name)}.max(0)as usize;
        let owner_class=String::from_utf16_lossy(&owner_name[..owner_length]);
        let request=Request{english:Some(zsdesk_core::i18n::is_english()),token:token.clone(),session:session.clone(),ordinal,parent:std::process::id(),parent_created,owner:owner.0 as isize,
            point:[point.x,point.y],pidls,folder,options,rename:intercept.rename,open:intercept.open,file_actions:intercept.file_actions,mode,extended,layout:origin.layout,view_context:origin.view_context,
            #[cfg(test)]probe:origin.probe,};
        let write_started=Instant::now();
        if let Err(error)=validate(&request).and_then(|_|write(&workspace.join("request.json"),&request)){
            if let Some(child)=child.take(){EventSet::signal(events.cancel);EventSet::signal(events.request);retire_child(child,job.take(),workspace.clone(),child_created,inherited_commit);}else{cleanup(&workspace);}return Err(error);
        }
        let mut timing=MenuTimings{reused_process:reused,request_write_microseconds:micros(write_started.elapsed()),..Default::default()};
        let identity=if !reused{
            let stderr=match fs::File::create(workspace.join("worker.stderr.log")){Ok(file)=>file,Err(error)=>{cleanup(&workspace);return Err(error.into());}};
            #[cfg(not(test))]let arguments=vec!["--context-menu-worker".into(),workspace.join("request.json").into_os_string()];
            #[cfg(test)]let arguments=vec!["context_menu_process::tests::native_extension_worker_entry".into(),"--ignored".into(),"--exact".into(),"--nocapture".into()];
            let spawn_started=Instant::now();let spawned=child::spawn(&std::env::current_exe()?,&arguments,&workspace,&session,&stderr);drop(stderr);
            let mut spawned=match spawned{Ok(child)=>child,Err(error)=>{cleanup(&workspace);return Err(error.into());}};
            let identity=created(spawned.id()).context("无法核对菜单进程创建时间");
            let boundary=(||->Result<usize>{
                let handle=unsafe{CreateJobObjectW(None,PCWSTR::null())}?;
                let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_PROCESS_MEMORY|JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK|JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
                limits.ProcessMemoryLimit=WORKER_MEMORY_LIMIT;
                if let Err(error)=unsafe{SetInformationJobObject(handle,JobObjectExtendedLimitInformation,(&limits as*const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),std::mem::size_of_val(&limits)as u32)
                    .and_then(|_|AssignProcessToJobObject(handle,HANDLE(spawned.as_raw_handle())))}{let _=unsafe{CloseHandle(handle)};return Err(error.into());}Ok(handle.0 as usize)
            })();
            let (identity,boundary)=match(identity,boundary){(Ok(identity),Ok(boundary))=>(identity,boundary),(identity,boundary)=>{
                let _=spawned.kill();let _=spawned.wait();let error=match boundary{Ok(boundary)=>{let _=unsafe{CloseHandle(HANDLE(boundary as*mut _))};identity.err().unwrap()},Err(error)=>identity.err().unwrap_or(error)};
                cleanup(&workspace);return Err(error);
            }};
            timing.process_spawn_microseconds=micros(spawn_started.elapsed());child=Some(spawned);job=Some(boundary);identity
        }else{child_created};
        // COM activation starts only after assignment to the bounded Job.
        EventSet::signal(events.request);
        if !mode.prewarm(){if let Some(child)=&child{let _=unsafe{AllowSetForegroundWindow(child.id())};}}
        Ok(Self{child,job,workspace,token,session,events:Some(events),child_created:identity,ordinal,thread,desktop,started,started_counter,inherited_commit,
            last_validation:0,committed:false,complete:false,status_dirty:true,last_status:None,timing,mode,item_count,owner_class,owner:MenuOwner::capture(owner),diagnostic_written:false})
    }
    pub fn pid(&self)->Option<u32>{self.child.as_ref().map(Child::id)}
    pub fn timings(&self)->Option<(u64,u64)>{self.last_status.as_ref().map(|status|(status.bind_microseconds,status.populate_microseconds))}
    pub fn created_items(&self)->Vec<zsdesk_windows::shell::new_menu::CreatedItem>{self.last_status.as_ref().map(|status|status.created.clone()).unwrap_or_default()}
    pub fn result_layout(&self)->Option<ShellViewState>{self.last_status.as_ref().and_then(|status|status.layout.clone())}
    pub fn result_sort_changed(&self)->bool{self.last_status.as_ref().is_some_and(|status|status.layout_sort_changed)}
    pub fn detailed_timings(&self)->MenuTimings{self.timing.clone()}
    /// Waits for a status transition or a queued UI/COM message, never for a
    /// fixed file-poll interval. The caller continues pumping its own STA.
    pub fn wait_for_progress(&mut self,milliseconds:u32){
        let Some(events)=&self.events else{return;};
        let mut handles=vec![events.status];if let Some(child)=&self.child{handles.push(child.as_raw_handle()as usize);}
        if wait_signal(&handles,milliseconds)==Some(0){self.status_dirty=true;}
    }
    fn read_status(&mut self){
        self.timing.status_reads=self.timing.status_reads.saturating_add(1);
        if let Ok(status)=read_small::<Status>(&self.workspace.join("status.json"),MAX_STATUS){
            if status.token==self.token&&self.pid()==Some(status.pid){
                self.timing.worker_thread=status.timing.worker_thread;
                self.timing.fault_report_available=status.timing.fault_report_available;
                self.timing.com_microseconds=status.timing.com_microseconds;self.timing.worker_setup_microseconds=status.timing.worker_setup_microseconds;
                self.timing.validation_microseconds=status.timing.validation_microseconds;self.timing.worker_validation_count=status.validation;
                self.timing.foreground_microseconds=status.timing.foreground_microseconds;
                self.last_status=Some(status);
            }
        }
        if self.timing.parent_to_menu_microseconds.is_none(){
            if let Ok(opened)=read_small::<MenuOpened>(&self.workspace.join("opened.json"),2048){
                if opened.token==self.token&&self.pid()==Some(opened.pid)&&opened.at>=self.started_counter{
                    self.timing.parent_to_menu_microseconds=Some(opened.at-self.started_counter);
                    self.timing.worker_to_menu_microseconds=Some(opened.at.saturating_sub(opened.worker_started));
                    self.timing.tracking_to_menu_microseconds=(opened.tracking_started!=0).then_some(opened.at.saturating_sub(opened.tracking_started));
                    self.timing.menu_open_observation=Some(opened.observation);
                }
            }
        }
    }
    fn record(&mut self,exit:Option<i32>,error:Option<&str>)->bool{
        if self.diagnostic_written{return false;}self.diagnostic_written=true;
        let _guard=DIAGNOSTICS_LOCK.lock().unwrap_or_else(|error|error.into_inner());
        let root=self.workspace.parent().unwrap_or(Path::new("."));
        let value=serde_json::json!({"schema":"ZSDesk.MenuDiagnostics.v1","timestamp_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|time|time.as_millis()as u64),
            "parent_pid":std::process::id(),"worker_pid":self.pid(),"worker_created":self.child_created,"owner_class":self.owner_class,"mode":self.mode,"selection_count":self.item_count,
            "phase":self.last_status.as_ref().map(|status|status.phase.as_str()).unwrap_or("launch"),"worker_exit_code":exit,
            "error_phase":self.last_status.as_ref().and_then(|status|status.error_phase.as_deref()),"error_hresult":self.last_status.as_ref().and_then(|status|status.error_hresult).map(|code|format!("0x{:08X}",code as u32)),
            "worker_exit_hex":exit.map(|code|format!("0x{:08X}",code as u32)),"committed":self.committed,
            "bind_microseconds":self.last_status.as_ref().map(|status|status.bind_microseconds),"populate_microseconds":self.last_status.as_ref().map(|status|status.populate_microseconds),
            "timing":self.timing,"native_menu":self.last_status.as_ref().map(|status|&status.native_menu),"fault":read_small::<serde_json::Value>(&self.workspace.join("fault.json"),16384).ok(),
            "error":error.map(redacted_error),"stderr":redacted_error(&worker_stderr(&self.workspace))});
        let crash_saved=save_crash(root,exit,&value);
        if self.mode.prewarm(){let _=write(&root.join("menu-prewarm.json"),&value);return crash_saved;}
        let current=root.join("menu-diagnostics.current.json");let previous=root.join("menu-diagnostics.previous.json");
        if let Ok(old)=read_small::<serde_json::Value>(&current,128*1024){let _=write(&previous,&old);}
        let _=write(&current,&value);
        crash_saved
    }
    pub fn poll(&mut self,before:&mut dyn FnMut(bool)->bool)->Option<Result<MenuOutcome>>{
        if self.complete{return None;}
        if self.status_dirty||self.events.as_ref().is_some_and(|events|EventSet::signaled(events.status)){self.status_dirty=false;self.read_status();}
        if let Some(status)=self.last_status.clone(){
            if status.phase=="validate"&&status.validation>self.last_validation{
                let started=Instant::now();let permit=before(status.before_invoke);self.last_validation=status.validation;
                self.timing.parent_validation_microseconds=self.timing.parent_validation_microseconds.saturating_add(micros(started.elapsed()));
                if permit&&status.before_invoke{self.committed=true;}
                if let Err(error)=write(&self.workspace.join("decision.json"),&Decision{token:self.token.clone(),validation:status.validation,permit}){
                    self.complete=true;self.record(None,Some(&error.to_string()));return Some(Err(error));
                }
                if let Some(events)=&self.events{EventSet::signal(events.decision);}
            }
            if matches!(status.phase.as_str(),"done"|"idle"){
                self.complete=true;self.record(None,status.error.as_deref());
                return Some(match &status.error{Some(_)=>Err(worker_failure(&status)),None=>Ok(status.outcome.unwrap_or(MenuOutcome::Dismissed))});
            }
        }
        if let Some(exit)=self.child.as_mut().and_then(|child|child.try_wait().ok().flatten()){
            self.read_status();self.complete=true;
            if !exit.success(){
                let code=exit.code();
                let detail=match code{
                    Some(c) if c as u32==0xc0000005=>"右键菜单进程发生内存访问冲突（0xC0000005）".to_owned(),
                    Some(c)=>format!("右键菜单进程异常退出（0x{:08X}）",c as u32),
                    None=>"右键菜单进程异常退出".to_owned(),
                };
                let saved=self.record(code,Some(&detail));
                let detail=if saved{format!("{detail}；已保存本次故障记录")}else{detail};
                return Some(Err(MenuFailure{phase:self.last_status.as_ref().map(|s|s.phase.clone()).unwrap_or_else(||"launch".into()),
                    code,detail,invoked:self.committed}.into()));
            }
            let error=if let Some(status)=self.last_status.as_ref().filter(|status|matches!(status.phase.as_str(),"done"|"idle")){status.error.clone()}
                else{Some("系统扩展菜单未返回完整结果，请重试".into())};
            self.record(exit.code(),error.as_deref());
            return Some(match error{Some(error)=>Err(self.last_status.as_ref().filter(|status|status.error.is_some()).map_or_else(||anyhow::anyhow!(error),worker_failure)),None=>Ok(self.last_status.as_ref().and_then(|status|status.outcome).unwrap_or(MenuOutcome::Dismissed))});
        }
        let showing=self.timing.parent_to_menu_microseconds.is_some();
        let preparation_limit=if matches!(self.mode,Mode::CatalogSelection|Mode::CatalogBackground){30}else{10};
        if !self.committed&&!showing&&self.started.elapsed()>Duration::from_secs(preparation_limit){
            self.complete=true;self.cancel();let message="系统扩展菜单加载超时；文件管理窗口仍可使用，诊断已保留";
            self.record(None,Some(message));return Some(Err(anyhow::anyhow!(message)));
        }
        None
    }
    pub fn cancel(&mut self){if let Some(events)=&self.events{EventSet::signal(events.cancel);EventSet::signal(events.request);}}
    /// Withdraw just this request. Only an idle acknowledgement without pending UI
    /// or clipboard ownership can return the worker to the pool.
    pub(crate) fn finish_dismissal(&mut self){
        if self.committed || !self.owner.alive(){self.cancel();return;}
        let Some(events)=&self.events else{return;};
        EventSet::signal(events.dismiss);
        if let Some(status)=&self.last_status{
            let hwnd=HWND(status.window as *mut _);
            let mut pid=0;
            if unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid))}!=0 && self.pid()==Some(pid){
                let _=unsafe{PostMessageW(Some(hwnd),DISMISS_REQUEST,WPARAM(0),LPARAM(0))};
            }
        }
        let until=Instant::now()+Duration::from_millis(800);
        while Instant::now()<until{
            if self.poll(&mut |_|false).is_some(){return;}
            self.wait_for_progress(15);
        }
        self.cancel();
    }
}
impl Drop for Client{fn drop(&mut self){
    // A worker that executed a command may serve the next menu: the worker
    // advertises "idle" only without visible windows or clipboard ownership,
    // and a committed process is never terminated, so keeping it warm avoids
    // a cold process and cold extension loads on the very next right-click.
    let committed=self.committed||self.inherited_commit;
    let reusable=self.complete&&self.ordinal<MAX_SESSION_REQUESTS
        &&self.last_status.as_ref().is_some_and(|status|status.phase=="idle"&&status.reusable&&status.error.is_none())
        &&self.child.as_mut().is_some_and(|child|child.try_wait().ok().flatten().is_none()&&created(child.id())==Some(self.child_created));
    if reusable{
        let idle=IdleWorker{child:self.child.take(),job:self.job.take(),workspace:self.workspace.clone(),session:self.session.clone(),token:self.token.clone(),
            events:self.events.take(),count:self.ordinal,created:self.child_created,last_used:Instant::now(),desktop:self.desktop,thread:self.thread,committed,
            prepared_drawn:self.mode.prewarm() && self.last_status.as_ref().is_some_and(|s|s.native_menu["drawn_renderer_ready"]==true),
            native_owner:self.last_status.as_ref().filter(|s|s.native_menu["runtime_initializations"].as_u64().unwrap_or(0)>0).map(|_|self.owner)};
        let retired={let mut pool=IDLE.lock().unwrap_or_else(|error|error.into_inner());let old=pool.iter().position(|worker|worker.thread==self.thread).map(|index|pool.swap_remove(index));
            let evicted=if pool.len()>=MAX_IDLE_WORKERS{Some(pool.remove(0))}else{None};pool.push(idle);(old,evicted)};drop(retired);start_reaper();return;
    }
    self.cancel();
    if let Some(child)=self.child.take(){retire_child(child,self.job.take(),self.workspace.clone(),self.child_created,committed);}
}}

enum AsyncEvent {Validate{before:bool,answer:std::sync::mpsc::SyncSender<bool>},Finished(Result<MenuOutcome>,Option<ShellViewState>,Vec<zsdesk_windows::shell::new_menu::CreatedItem>,Vec<zsdesk_windows::shell::menu::MenuCatalogItem>)}
/// Posted to an [`AsyncClient`] owner whenever a validation barrier or the
/// result is waiting, so the owner answers at once instead of on its next
/// polling tick (each barrier otherwise added up to one timer period).
pub fn progress_message()->u32{
    static MESSAGE:std::sync::OnceLock<u32>=std::sync::OnceLock::new();
    *MESSAGE.get_or_init(||unsafe{RegisterWindowMessageW(w!("ZSDesk.ContextMenuProgress"))})
}
fn notify_owner(owner:isize){
    let message=progress_message();
    if message!=0{let _=unsafe{PostMessageW(Some(HWND(owner as*mut _)),message,WPARAM(0),LPARAM(0))};}
}
/// Preparation, IPC and diagnostic I/O run off the window STA. Only the exact
/// selection validation barrier is delivered back to that STA.
pub struct AsyncClient {
    catalog:Vec<zsdesk_windows::shell::menu::MenuCatalogItem>,
    created:Vec<zsdesk_windows::shell::new_menu::CreatedItem>,
    events:std::sync::mpsc::Receiver<AsyncEvent>,
    cancel:std::sync::Arc<std::sync::atomic::AtomicBool>,
    pid:std::sync::Arc<std::sync::atomic::AtomicU32>,complete:bool,
    showing:std::sync::Arc<std::sync::atomic::AtomicBool>,
    owner:isize,returned:Option<std::sync::mpsc::Receiver<()>>,
    layout:Option<ShellViewState>,
}
// A follow-up on the same window can reuse its closing worker. Only the protocol
// thread waits for the old transaction, never the foreground window procedure.
thread_local! {
    static DISMISSING:std::cell::RefCell<Option<(isize,std::sync::mpsc::Receiver<()>)>>=const{std::cell::RefCell::new(None)};
}
impl AsyncClient {
    pub fn spawn(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode)->Result<Self>{
        Self::spawn_extended(owner,point,pidls,folder,options,intercept,mode,false)
    }
    pub fn spawn_extended(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool)->Result<Self>{
        Self::spawn_with_layout(owner,point,pidls,folder,options,intercept,mode,extended,None)
    }
    pub fn spawn_with_layout(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool,layout:Option<ShellViewState>)->Result<Self>{
        Self::spawn_with_context(owner,point,pidls,folder,options,intercept,mode,extended,layout,None)
    }
    pub fn spawn_with_context(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool,layout:Option<ShellViewState>,view_context:Option<MenuViewContext>)->Result<Self>{
        use std::sync::{Arc,atomic::{AtomicBool,AtomicU32},mpsc};
        ensure!(PREPARING.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<4).then_some(n+1)).is_ok(),"菜单正在处理其他请求，请稍后重试");
        struct Active;impl Drop for Active{fn drop(&mut self){PREPARING.fetch_sub(1,Ordering::AcqRel);}}
        let active=Active;
        let mut origin=Origin::capture();origin.layout=layout;origin.view_context=view_context;let owner=owner.0 as isize;let coordinates=(point.x,point.y);
        let (sender,events)=mpsc::sync_channel(4);let cancel=Arc::new(AtomicBool::new(false));let pid=Arc::new(AtomicU32::new(0));
        let showing=Arc::new(AtomicBool::new(false));let shown=showing.clone();
        let stop=cancel.clone();let process=pid.clone();
        let prior=DISMISSING.with(|slot|slot.borrow_mut().take_if(|(window,_)|*window==owner));
        let (return_guard,returned)=mpsc::channel::<()>();
        std::thread::Builder::new().name("zsdesk-menu-protocol".into()).spawn(move||{
            let _active=active;
            let _return_guard=return_guard;
            if let Some((_,previous))=prior{let _=previous.recv_timeout(Duration::from_millis(250));}
            if stop.load(Ordering::Acquire){return;}
            let client=Client::spawn_at(HWND(owner as *mut _),POINT{x:coordinates.0,y:coordinates.1},pidls,folder,options,intercept,mode,extended,origin);
            let mut client=match client{Ok(client)=>client,Err(error)=>{let _=sender.try_send(AsyncEvent::Finished(Err(error),None,Vec::new(),Vec::new()));notify_owner(owner);return;}};
            process.store(client.pid().unwrap_or(0),Ordering::Release);
            loop {
                if stop.load(Ordering::Acquire){client.finish_dismissal();break;}
                let result=client.poll(&mut|before|{
                    let (answer,receive)=mpsc::sync_channel(1);
                    if sender.try_send(AsyncEvent::Validate{before,answer}).is_err(){return false;}
                    notify_owner(owner);
                    let deadline=Instant::now()+Duration::from_secs(2);
                    loop {
                        if stop.load(Ordering::Acquire)||Instant::now()>=deadline{return false;}
                        match receive.recv_timeout(Duration::from_millis(20)) {
                            Ok(permit)=>return permit&&!stop.load(Ordering::Acquire),
                            Err(mpsc::RecvTimeoutError::Disconnected)=>return false,
                            Err(mpsc::RecvTimeoutError::Timeout)=>{},
                        }
                    }
                });
                shown.store(client.timing.parent_to_menu_microseconds.is_some(),Ordering::Release);
                if let Some(result)=result {
                    let layout=client.last_status.as_ref().and_then(|status|status.layout.clone());
                    let created=client.created_items();
                    let catalog=client.last_status.as_mut().map(|s|std::mem::take(&mut s.catalog)).unwrap_or_default();
                    drop(client);
                    let _=sender.try_send(AsyncEvent::Finished(result,layout,created,catalog));notify_owner(owner);break;
                }
                client.wait_for_progress(20);
            }
        }).context("无法启动菜单通信线程")?;
        Ok(Self{events,cancel,pid,showing,complete:false,owner,returned:Some(returned),layout:None,created:Vec::new(),catalog:Vec::new()})
    }
    pub fn preparing(&self)->bool{!self.complete&&!self.cancel.load(Ordering::Acquire)&&!self.showing.load(Ordering::Acquire)}
    pub fn pid(&self)->Option<u32>{let pid=self.pid.load(Ordering::Acquire);(pid!=0).then_some(pid)}
    pub fn take_layout(&mut self)->Option<ShellViewState>{self.layout.take()}
    pub fn take_created(&mut self)->Vec<zsdesk_windows::shell::new_menu::CreatedItem>{std::mem::take(&mut self.created)}
    pub fn take_catalog(&mut self)->Vec<zsdesk_windows::shell::menu::MenuCatalogItem>{std::mem::take(&mut self.catalog)}
    pub fn poll(&mut self,before:&mut dyn FnMut(bool)->bool)->Option<Result<MenuOutcome>> {
        if self.complete{return None;}
        for _ in 0..4 {match self.events.try_recv() {
            Ok(AsyncEvent::Validate{before:invoke,answer})=>{let permit=!self.cancel.load(Ordering::Acquire)&&before(invoke);let _=answer.try_send(permit);},
            Ok(AsyncEvent::Finished(result,layout,created,catalog))=>{self.complete=true;self.layout=layout;self.created=created;self.catalog=catalog;return Some(result);},
            Err(std::sync::mpsc::TryRecvError::Empty)=>return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected)=>{self.complete=true;return Some(Err(anyhow::anyhow!("菜单通信已结束，请重新打开菜单")));},
        }}None
    }
}
impl Drop for AsyncClient {fn drop(&mut self){
    self.cancel.store(true,Ordering::Release);
    if !self.complete&&self.pid().is_some(){
        if let Some(returned)=self.returned.take(){
            let _=DISMISSING.try_with(|slot|*slot.borrow_mut()=Some((self.owner,returned)));
        }
    }
}}

struct Worker<'a>{request:Request,path:PathBuf,status:Status,events:&'a EventSet}
struct Watch{
    #[cfg(test)]visual_step:std::cell::Cell<usize>,
    request:Request,path:PathBuf,status_event:usize,cancel_event:usize,dismiss_event:usize,worker_started:u64,
    menu_open:std::cell::Cell<bool>,
    tracking_started:std::cell::Cell<u64>,
    #[cfg(test)]command:std::cell::Cell<Option<u32>>,
    #[cfg(test)]classic_loop:std::cell::Cell<bool>,
    #[cfg(test)]expanded_requested:std::cell::Cell<bool>,
    #[cfg(test)]classic_commands:std::cell::Cell<u64>,
    #[cfg(test)]menu:std::cell::Cell<HMENU>,
}
extern "C"{fn zs_menu_is_open(owner:HWND)->i32;}
#[cfg(test)]extern "C"{fn zs_menu_command_messages()->u64;}
#[cfg(test)]extern "C"{fn zs_menu_test_request_expanded(owner:HWND,point:POINT)->windows::core::HRESULT;}
impl Worker<'_>{
    fn state(&mut self,phase:&str)->Result<()>{
        self.status.phase=phase.into();self.status.native_menu=serde_json::to_value(zsdesk_windows::shell::menu_diagnostics())?;
        self.status.native_menu["presentation"]=zsdesk_windows::shell::menu::presentation_name().into();
        self.status.native_menu["drawn_renderer_ready"]=zsdesk_windows::shell::menu::drawn_renderer_ready().into();
        write(&self.path.with_file_name("status.json"),&self.status)?;EventSet::signal(self.events.status);Ok(())
    }
    fn allow(&mut self,before_invoke:bool)->windows::core::Result<()>{
        let started=Instant::now();
        let result=(||->Result<()>{
            ensure!(parent_alive(&self.request)&&!EventSet::signaled(self.events.cancel)&&!EventSet::signaled(self.events.dismiss),"菜单已取消");
            self.status.validation+=1;self.status.before_invoke=before_invoke;self.state("validate")?;
            let deadline=Instant::now()+Duration::from_secs(3);
            loop{
                if wait_signal(&[self.events.decision,self.events.cancel],50)==Some(0){
                    if let Ok(answer)=read_small::<Decision>(&self.path.with_file_name("decision.json"),65536){
                        if answer.token==self.request.token&&answer.validation==self.status.validation{ensure!(answer.permit&&!EventSet::signaled(self.events.dismiss),"菜单选择已改变");break;}
                    }
                }
                ensure!(!EventSet::signaled(self.events.cancel)&&!EventSet::signaled(self.events.dismiss)&&Instant::now()<deadline&&owner_alive(&self.request),"菜单已取消");
                pump(0);
            }
            self.status.timing.validation_microseconds=self.status.timing.validation_microseconds.saturating_add(micros(started.elapsed()));
            self.state(if before_invoke{"invoking"}else{"tracking"})?;Ok(())
        })();result.map_err(|error|windows::core::Error::new(windows::core::HRESULT(0x80004004u32 as i32),error.to_string()))
    }
}
pub(crate) fn foreground_is_menu_worker(worker:Option<u32>,owner:HWND)->bool{
    let Some(pid)=worker else{return false;};
    let foreground=unsafe{GetForegroundWindow()};
    let mut actual=0;
    unsafe{GetWindowThreadProcessId(foreground,Some(&mut actual));}
    if actual!=pid || unsafe{GetWindow(foreground,GW_OWNER)}.ok()!=Some(unsafe{GetAncestor(owner,GA_ROOT)}){return false;}
    let mut class=[0u16;64];
    let count=unsafe{GetClassNameW(foreground,&mut class)}.max(0)as usize;
    class[..count].iter().copied().eq("ZSDesk.ExtensionMenu.Worker".encode_utf16())
}
fn owner_alive(request:&Request)->bool{
    let mut pid=0;let owner=HWND(request.owner as*mut _);unsafe{GetWindowThreadProcessId(owner,Some(&mut pid));}
    pid==request.parent&&unsafe{IsWindow(Some(owner))}.as_bool()
}
fn pump(ms:u64){
    let end=Instant::now()+Duration::from_millis(ms);
    loop{
        let mut message=MSG::default();for _ in 0..64{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}
        if Instant::now()>=end{break;}let _=wait_signal(&[],(end.saturating_duration_since(Instant::now()).as_millis()as u32).min(10));
    }
}
fn classic_menu_visible()->bool{
    let mut visible=false;
    unsafe extern "system" fn each(hwnd:HWND,data:LPARAM)->windows::core::BOOL{
        let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
        if pid==std::process::id()&&unsafe{IsWindowVisible(hwnd)}.as_bool(){
            let mut name=[0u16;32];let size=unsafe{GetClassNameW(hwnd,&mut name)}.max(0)as usize;
            if String::from_utf16_lossy(&name[..size])=="#32768"{unsafe{*(data.0 as*mut bool)=true;}return false.into();}
        }true.into()
    }
    let _=unsafe{EnumWindows(Some(each),LPARAM((&mut visible as*mut bool)as isize))};visible
}
impl Watch{
    fn observe_open(&self,hwnd:HWND){
        if self.menu_open.get(){return;}
        let observation=if zsdesk_windows::flyout::is_open(){Some("common-command-flyout")}
            else if zsdesk_windows::shell::menu::drawn_menu_window().is_some_and(|window|unsafe{IsWindowVisible(window)}.as_bool()){Some("drawn-win11-menu")}
            else if self.request.options.modern&&unsafe{zs_menu_is_open(hwnd)}!=0{Some("native-presenter-open")}
            else if classic_menu_visible(){Some("visible-classic-menu")}else{None};
        #[cfg(test)]let observation=observation.or_else(||(self.classic_loop.get()&&self.request.probe.as_deref()==Some("modern-owner-copy")).then_some("classic-menu-entered"));
        if let Some(observation)=observation{
            self.menu_open.set(true);
            #[cfg(test)]if self.request.probe.as_deref()==Some("menu-dismiss"){unsafe{SetTimer(Some(hwnd),2,80,None);}}
            let value=MenuOpened{token:self.request.token.clone(),pid:std::process::id(),at:monotonic_us(),worker_started:self.worker_started,observation:observation.into(),tracking_started:self.tracking_started.get()};
            if write(&self.path.with_file_name("opened.json"),&value).is_ok(){EventSet::signal(self.status_event);}
            unsafe{SetTimer(Some(hwnd),1,100,None);}
        }
    }
}
#[cfg(test)]
fn visual_probe(state:&Watch){
    let Some(root)=zsdesk_windows::shell::menu::drawn_menu_window()else{return;};
    let Some(output)=std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").map(PathBuf::from)else{return;};
    let probe=state.request.probe.as_deref().unwrap_or_default();
    let snap=zsdesk_windows::shell::menu::drawn_menu_snapshot();
    let Some(levels)=snap["levels"].as_array()else{return;};
    if levels.is_empty(){return;}
    let _=write(&output.join(format!("{probe}.json")),&snap);
    let path:&[&str]=match probe{"visual-new"=>&["新建"],"visual-seven"|"visual-legacy"=>&["7-zip"],"visual-deep"=>&["7-zip","crc"],
        "visual-archive"=>&["7-zip","one.7z"],"visual-more"=>&["显示更多选项"],
        "visual-more-zip"=>&["显示更多选项","7-zip"],"visual-more-archive"=>&["显示更多选项","7-zip","sevenzip","two.7z"],
        "visual-openwith"=>&["打开方式"],_=>&[]};
    let depth=levels.len()-1;
    if depth>=path.len(){let _=zsdesk_windows::shell::menu::dismiss_active_menu(root);return;}
    if state.visual_step.get()>depth{return;}
    let lvl=&levels[depth];
    let Some(rows)=lvl["rows"].as_array()else{return;};
    let Some(row)=rows.iter().find(|row|row["enabled"]==true&&row["height"].as_f64().unwrap_or(0.)>0.&&
        row["label"].as_str().is_some_and(|v|v.to_lowercase().contains(path[depth])))else{return;};
    let Some(bounds)=lvl["bounds"].as_array()else{return;};
    let (Some(x),Some(y),Some(row_y),Some(height))=(bounds[0].as_i64(),bounds[1].as_i64(),row["y"].as_f64(),row["height"].as_f64())else{return;};
    let mut point=POINT{x:x as i32+40,y:y as i32+(row_y+height/2.-lvl["scroll"].as_f64().unwrap_or(0.))as i32};
    if !unsafe{windows::Win32::Graphics::Gdi::ScreenToClient(root,&mut point)}.as_bool(){return;}
    let packed=LPARAM((point.x as u16 as u32|((point.y as u16 as u32)<<16))as isize);
    state.visual_step.set(depth+1);
    unsafe{let _=PostMessageW(Some(root),WM_MOUSEMOVE,WPARAM(0),packed);
        let _=PostMessageW(Some(root),WM_LBUTTONDOWN,WPARAM(1),packed);
        let _=PostMessageW(Some(root),WM_LBUTTONUP,WPARAM(0),packed);}
}
const DISMISS_REQUEST:u32=WM_APP+0x621;
unsafe extern "system" fn procedure(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if msg==WM_NCCREATE{let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize);}return LRESULT(1);}
    let state=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const Watch;
    if !state.is_null(){let state=unsafe{&*state};
        if matches!(msg,WM_ENTERMENULOOP|WM_INITMENUPOPUP){unsafe{SetTimer(Some(hwnd),1,15,None);}
            #[cfg(test)]if msg==WM_ENTERMENULOOP{state.classic_loop.set(true);}
            #[cfg(test)]if msg==WM_INITMENUPOPUP&&state.request.probe.as_deref()==Some("modern-expand-copy")&&state.classic_loop.get(){
                state.menu.set(HMENU(wp.0 as*mut _));state.command.set(zsdesk_windows::shell::menu::active_command_by_verb("copy"));
            }
            #[cfg(test)]if state.request.probe.as_deref()==Some("menu-dismiss")||(state.request.probe.as_deref()==Some("modern-expand")&&msg==WM_ENTERMENULOOP){unsafe{SetTimer(Some(hwnd),2,80,None);}}
        }
        #[cfg(test)]if msg==WM_TIMER&&wp.0==2{let _=unsafe{KillTimer(Some(hwnd),2)};let _=zsdesk_windows::shell::menu::dismiss_active_menu(hwnd);let _=unsafe{EndMenu()};return LRESULT(0);}
        if msg==WM_TIMER||msg==DISMISS_REQUEST{
            if !owner_alive(&state.request)||EventSet::signaled(state.cancel_event)||EventSet::signaled(state.dismiss_event){zsdesk_windows::flyout::cancel();let _=zsdesk_windows::shell::menu::dismiss_active_menu(hwnd);let _=unsafe{EndMenu()};}
            if msg==DISMISS_REQUEST{return LRESULT(0);}
            state.observe_open(hwnd);
            #[cfg(test)]if state.request.probe.as_deref().is_some_and(|p|p.starts_with("visual-"))&&zsdesk_windows::shell::menu::drawn_menu_open(){
                visual_probe(state);
            }
            #[cfg(test)]if matches!(state.request.probe.as_deref(),Some("modern-expand"|"modern-expand-copy"))&&state.menu_open.get()&&!state.expanded_requested.replace(true){
                unsafe{SetTimer(Some(hwnd),2,2000,None);}
                let result=unsafe{zs_menu_test_request_expanded(hwnd,POINT{x:state.request.point[0].saturating_add(43),y:state.request.point[1].saturating_add(89)})};
                if result.is_err(){eprintln!("expanded probe failed: {result:?}");}
            }
            #[cfg(test)]if state.request.probe.as_deref()==Some("modern-owner-extension")&&state.menu_open.get()&&!state.expanded_requested.replace(true){
                unsafe extern "C"{fn zs_menu_test_extension_id()->u32;}
                let command=unsafe{zs_menu_test_extension_id()};if command!=0{state.command.set(Some(command));}
            }
            #[cfg(test)]if matches!(state.request.probe.as_deref(),Some("modern-owner-copy"|"modern-expand-copy"|"modern-owner-new-folder"|"modern-owner-static-probe"|"modern-owner-properties"|"modern-owner-extension"))&&state.menu_open.get(){
                if let Some(command)=state.command.take(){
                    if state.classic_loop.get()&&unsafe{zs_menu_is_open(hwnd)}==0&&matches!(state.request.probe.as_deref(),Some("modern-owner-copy"|"modern-expand-copy")){
                        let mut label=[0u16;1024];let count=unsafe{GetMenuStringW(state.menu.get(),command,Some(&mut label),MF_BYCOMMAND)}.max(0)as usize;
                        let mut mnemonic=None;let mut index=0;
                        while index+1<count{if label[index]==b'&' as u16{if label[index+1]==b'&' as u16{index+=2;continue;}mnemonic=Some(label[index+1]);break;}index+=1;}
                        if let Some(key)=mnemonic{if unsafe{PostMessageW(Some(hwnd),WM_CHAR,WPARAM(key as usize),LPARAM(1))}.is_ok(){state.classic_commands.set(state.classic_commands.get()+1);}}
                        else{eprintln!("classic copy fixture has no keyboard mnemonic");let _=unsafe{EndMenu()};}
                    }else{let message=unsafe{RegisterWindowMessageW(w!("FILE_EXPLORER_CONTEXTMENU_INVOKEMENUITEM"))};
                        if message!=0{let _=unsafe{PostMessageW(Some(hwnd),message,WPARAM(command as usize),LPARAM(0))};}}
                }
            }
        }
    }
    if let Some(result)=forward_menu_message(msg,wp,lp){return result;}
    unsafe{DefWindowProcW(hwnd,msg,wp,lp)}
}
fn run_request(path:&Path,request:Request,events:&EventSet,com_us:u64,fault_report_available:bool)->Result<bool>{
    let _language=request.english.map(zsdesk_core::i18n::Scope::english);
    let setup=Instant::now();let worker_started=monotonic_us();
    let mut state=Box::new(Worker{status:Status{token:request.token.clone(),pid:std::process::id(),window:0,phase:"starting".into(),validation:0,before_invoke:false,outcome:None,error:None,error_hresult:None,error_phase:None,bind_microseconds:0,populate_microseconds:0,
        reusable:false,layout:None,layout_sort_changed:false,created:Vec::new(),catalog:Vec::new(),timing:MenuTimings{worker_thread:unsafe{windows::Win32::System::Threading::GetCurrentThreadId()},fault_report_available,com_microseconds:com_us,..Default::default()},native_menu:serde_json::Value::Null,
        #[cfg(test)]probe_menu_open:false,#[cfg(test)]probe_classic_loop:false,#[cfg(test)]probe_expanded_requested:false,#[cfg(test)]probe_command_messages:0,#[cfg(test)]probe_clipboard_after_invoke:serde_json::Value::Null},request,path:path.into(),events});
    state.state("starting")?;
    #[cfg(test)]if state.request.probe.as_deref()==Some("crash"){unsafe{RaiseException(0xc0000005,1,0,std::ptr::null());}std::process::exit(0x51);}
    #[cfg(test)]if state.request.probe.as_deref()==Some("memory-limit"){
        use windows::Win32::System::Memory::{VirtualAlloc,VirtualFree,MEM_COMMIT,MEM_RESERVE,MEM_RELEASE,PAGE_READWRITE};
        let memory=unsafe{VirtualAlloc(None,WORKER_MEMORY_LIMIT+4096,MEM_COMMIT|MEM_RESERVE,PAGE_READWRITE)};
        if !memory.is_null(){let _=unsafe{VirtualFree(memory,0,MEM_RELEASE)};}
        ensure!(memory.is_null(),"worker exceeded configured memory commitment boundary");
        state.status.outcome=Some(MenuOutcome::Dismissed);state.state("done")?;return Ok(false);
    }
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}.context("menu owner module")?.0);let class=w!("ZSDesk.ExtensionMenu.Worker");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(procedure),lpszClassName:class,..Default::default()});}
    let watch=Box::new(Watch{#[cfg(test)]visual_step:std::cell::Cell::new(0),request:state.request.clone(),path:path.into(),status_event:events.status,cancel_event:events.cancel,dismiss_event:events.dismiss,
        worker_started,menu_open:std::cell::Cell::new(false),tracking_started:std::cell::Cell::new(0),#[cfg(test)]command:std::cell::Cell::new(None),
        #[cfg(test)]classic_loop:std::cell::Cell::new(false),#[cfg(test)]expanded_requested:std::cell::Cell::new(false),#[cfg(test)]classic_commands:std::cell::Cell::new(0),#[cfg(test)]menu:std::cell::Cell::new(HMENU::default())});
    let owner=unsafe{GetAncestor(HWND(state.request.owner as*mut _),GA_ROOT)};
    ensure!(owner_alive(&state.request),"菜单所属窗口已关闭");
    let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW,class,w!("ZSdesk"),WS_POPUP,0,0,0,0,Some(owner),None,Some(instance),Some((&*watch as*const Watch).cast()))}.context("create menu owner window")?;
    struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let owned_window=Window(window);
    state.status.window=window.0 as isize;unsafe{SetTimer(Some(window),1,15,None);}
    state.status.timing.worker_setup_microseconds=micros(setup.elapsed());
    let result=(||->Result<MenuOutcome>{
        let request=state.request.clone();zsdesk_windows::shell::menu::set_context_menu_options(request.options.clone());
        if request.mode.prewarm(){
            let effective=zsdesk_windows::shell::menu::context_menu_options();
            if effective.drawn {
                zsdesk_windows::shell::menu::prepare_drawn_runtime()?;
                if request.folder.is_some() && effective.native_first_page_apps.as_ref().is_some_and(|apps| !apps.is_empty()) {
                    if let Some(folder)=request.view_context.as_ref().and_then(|context| context.folder.as_deref()) {
                        background_view::prepare_folder(folder,&request,events)?;
                    }
                }
            }
            return Ok(MenuOutcome::Dismissed);
        }
        #[cfg(test)]if request.probe.as_deref()==Some("dismiss"){state.allow(false)?;return Ok(MenuOutcome::Dismissed);}
        #[cfg(test)]if request.probe.as_deref()==Some("retirement-dialog"){
            state.allow(false)?;state.allow(true)?;
            return Ok(MenuOutcome::NativeHandled);
        }
        let mut properties=request.mode==Mode::Properties;
        if common::enabled(&request){
            let first=common::specification(&request);
            state.allow(false)?;
            unsafe{let _=ShowWindow(window,SW_SHOWNOACTIVATE);let _=SetForegroundWindow(window);}
            if unsafe{GetForegroundWindow()}!=window{return Ok(MenuOutcome::Dismissed);}
            watch.tracking_started.set(monotonic_us());
            let anchor=crate::flyout_menu::MenuAnchor::Point{x:request.point[0],y:request.point[1]};
            let choice=crate::flyout_menu::show(window,&first,anchor,zsdesk_windows::theme::menus_dark());
            match choice.as_deref(){
                Some("more")=>{},
                Some("properties")=>properties=true,
                Some(id)=>{
                    if let Some(action)=common::outcome(id){
                        state.allow(true)?;
                        return Ok(action);
                    }
                    return Ok(MenuOutcome::Dismissed);
                },
                None=>return Ok(MenuOutcome::Dismissed),
            }
        }
        if properties{
            state.allow(true)?;
            let pidls=if request.pidls.is_empty(){vec![zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(request.folder.clone().context("目录缺失")?)).context("目录标识无效")?]}else{request.pidls.clone()};
            let data=zsdesk_windows::desktop::data_object(window,&pidls)?;
            unsafe{windows::Win32::UI::Shell::SHMultiFileProperties(&data,0)}?;
            return Ok(MenuOutcome::ShellInvoked);
        }
        state.state("binding")?;let started=Instant::now();
        let is_background=matches!(request.mode,Mode::Background|Mode::New|Mode::CatalogBackground|Mode::PasteBackground|Mode::PrepareBackground)||(request.mode==Mode::Paste&&request.pidls.is_empty());
        let background=if is_background{
            Some(if let Some(pidl)=request.pidls.first(){zsdesk_windows::shell::pidl::item_from_pidl(pidl)?}
                else{unsafe{zsdesk_windows::shell::shell_item(&zsdesk_core::model::explorer::ShellLocation::Path(request.folder.clone().context("粘贴目标缺失")?))}?})
        }else{None};
        let effective_options=zsdesk_windows::shell::menu::context_menu_options();
        let classic_selection=zsdesk_windows::theme::os_build()<22000&&!effective_options.modern&&!effective_options.drawn
            &&!request.pidls.is_empty()&&!is_background;
        let compact_selection=background_view::compact_selection(window,&request,&effective_options)?;
        let background_view=if compact_selection.is_some(){None}
            else if let Some(item)=&background{Some(background_view::BackgroundView::create(item,&request,events)?)}
            else if !request.pidls.is_empty()&&!classic_selection{
                let origin=background_view::selection_origin(&request);
                Some(background_view::for_selection(&request.pidls,origin,&request,events)?)
            }else{None};
        state.status.timing.background_view_reused=is_background&&background_view.is_some()&&background_view::reused();
        if is_background{if let (Some(host),Some(layout))=(&background_view,&request.layout){zsdesk_windows::shell::restore_view_layout(&host.view,layout);}}
        let initial_layout=background_view.as_ref().filter(|_|is_background).and_then(|host|zsdesk_windows::shell::capture_background_layout(&host.view));
        let full_context=matches!(request.mode,Mode::More|Mode::Background|Mode::CatalogSelection|Mode::CatalogBackground);
        let classic_gesture=request.extended&&!request.view_context.as_ref().is_some_and(|context|context.keyboard&&context.f10);
        let packaged=full_context&&(effective_options.drawn||!effective_options.modern||classic_gesture||matches!(request.mode,Mode::CatalogSelection|Mode::CatalogBackground));
        let menu=if let Some(menu)=compact_selection{menu}else if let Some(host)=&background_view{
            if request.mode==Mode::New{zsdesk_windows::shell::new_menu::for_view(&host.view)?}
            else if effective_options.drawn&&!classic_gesture&&matches!(request.mode,Mode::More|Mode::Background){
                ShellMenu::for_compact_view(&host.view,&host.browser,window,!is_background,&effective_options)?
            }
            else if packaged{ShellMenu::for_view_with_packages(&host.view,&host.browser,window,!is_background)?}
            else{ShellMenu::for_view(&host.view,!is_background)?}
        }else if request.pidls.is_empty(){let folder=zsdesk_windows::desktop::folder_for_path(request.folder.as_deref().context("目录缺失")?)?;ShellMenu::for_folder_background(window,&folder)?}else{ShellMenu::for_items(window,&request.pidls)?};
        menu.set_directory(request.folder.as_deref());
        if let Some(context)=&request.view_context{menu.set_input_context(context.keyboard,context.f10);}
        // Creation selection feedback is optional for generic background
        // handlers. A missing site must not make their entire menu unusable.
        let creation=if let Some(host)=background_view.as_ref().filter(|_|is_background&&request.folder.is_some()){
            match zsdesk_windows::shell::new_menu::CreationCapture::attach(&menu,&host.view){
                Ok(capture)=>Some(capture),
                Err(error) if request.mode!=Mode::New&&matches!(error.code().0 as u32,0x80004002|0x80004001)=>None,
                Err(error)=>return Err(error.into()),
            }
        }else{None};
        state.status.bind_microseconds=started.elapsed().as_micros()as u64;state.state("populating")?;let started=Instant::now();
        if matches!(request.mode,Mode::Open|Mode::Paste|Mode::PasteBackground){
            menu.populate_for_invoke(request.extended)?;state.status.populate_microseconds=started.elapsed().as_micros()as u64;
            let verb=if matches!(request.mode,Mode::Paste|Mode::PasteBackground){"paste"}else{"open"};
            state.allow(true)?;menu.invoke_verb(window,verb,false)?;return Ok(MenuOutcome::ShellInvoked);
        }
        menu.populate(request.rename,request.extended)?;
        #[cfg(test)]watch.menu.set(menu.hmenu());
        state.status.populate_microseconds=started.elapsed().as_micros()as u64;
        #[cfg(test)]if request.probe.as_deref()==Some("query-compact-catalog"){
            state.allow(false)?;state.status.catalog=menu.catalog();return Ok(MenuOutcome::Dismissed);
        }
        if matches!(request.mode,Mode::CatalogSelection|Mode::CatalogBackground){
            state.allow(false)?;state.status.catalog=menu.catalog();return Ok(MenuOutcome::Dismissed);
        }
        state.allow(false)?;
        #[cfg(test)]if matches!(request.probe.as_deref(),Some("modern-owner-extension"|"modern-ui-nested-extension")){
            use windows::{core::{GUID,Interface},Win32::{System::Com::{CoCreateInstance,CLSCTX_INPROC_SERVER},UI::Shell::IExplorerCommand}};
            unsafe extern "C"{fn zs_menu_test_extension(command:*mut core::ffi::c_void);}
            let command:IExplorerCommand=unsafe{CoCreateInstance(&GUID::from_u128(0xb3489bd3_6c40_4aaa_9610_d72754252627),None,CLSCTX_INPROC_SERVER)}?;
            unsafe{zs_menu_test_extension(command.as_raw());}
        }
        #[cfg(test)]if request.probe.as_deref()==Some("properties-direct"){
            state.allow(true)?;menu.invoke_verb(window,"properties",false)?;return Ok(MenuOutcome::ShellInvoked);
        }
        #[cfg(test)]if request.probe.as_deref()==Some("modern-owner-copy"){
            ensure!(request.options.modern,"modern owner probe requires the native Windows 11 presenter");
            let count=unsafe{GetMenuItemCount(Some(menu.hmenu()))};
            let command=(0..count).map(|index|unsafe{GetMenuItemID(menu.hmenu(),index)})
                .find(|id|menu.verb(*id).is_some_and(|verb|verb.eq_ignore_ascii_case("copy"))).context("native copy command missing")?;
            watch.command.set(Some(command));
        }
        #[cfg(test)]if request.probe.as_deref()==Some("modern-owner-new-folder"){
            ensure!(request.options.modern,"modern creation probe requires the native presenter");
            watch.command.set(Some(isolation_tests::new_item_command(&menu,false)?));
        }
        #[cfg(test)]if request.probe.as_deref()==Some("modern-owner-properties"){
            let command=(0..unsafe{GetMenuItemCount(Some(menu.hmenu()))})
                .map(|index|unsafe{GetMenuItemID(menu.hmenu(),index)})
                .find(|id|menu.verb(*id).is_some_and(|verb|verb.eq_ignore_ascii_case("properties")))
                .context("native properties command missing")?;
            watch.command.set(Some(command));
        }
        #[cfg(test)]if request.probe.as_deref()==Some("modern-owner-static-probe"){
            watch.command.set(Some(isolation_tests::static_background_command(&menu)?));
        }
        let foreground_started=Instant::now();
        unsafe{let _=ShowWindow(window,SW_SHOWNOACTIVATE);let _=SetForegroundWindow(window);}
        state.status.timing.foreground_microseconds=micros(foreground_started.elapsed());
        let point=POINT{x:request.point[0],y:request.point[1]};
        #[cfg(test)]let probe_outcome=if matches!(request.probe.as_deref(),Some("new-folder"|"new-text")){
            let command=isolation_tests::new_item_command(&menu,request.probe.as_deref()==Some("new-text"))?;
            state.allow(true)?;menu.invoke(window,command,None,false,false)?;Some(MenuOutcome::ShellInvoked)
        }else if request.probe.as_deref()==Some("background-static-probe"){
            let command=isolation_tests::static_background_command(&menu)?;
            state.allow(true)?;menu.invoke(window,command,None,false,false)?;Some(MenuOutcome::ShellInvoked)
        }else if request.probe.as_deref()==Some("background-details"){
            ensure!(background_view.is_some(),"background view required");let command=isolation_tests::background_details_command(&menu)?;
            state.allow(true)?;menu.invoke(window,command,None,false,false)?;Some(MenuOutcome::ShellInvoked)
        }else{None};
        #[cfg(not(test))]let probe_outcome:Option<MenuOutcome>=None;
        watch.tracking_started.set(monotonic_us());
        let file_actions=request.file_actions&&(is_background||
            zsdesk_windows::shell::file_transfer::paths_from_pidls(&request.pidls).is_ok());
        let outcome=if let Some(outcome)=probe_outcome{outcome}
            else if request.mode==Mode::New{menu.track_new(window,point,&mut||state.allow(true))?}
            else if let (Some(item),Some(host))=(&background,&background_view){menu.track_background_view_with_interceptions(window,point,item,&host.browser,MenuInterceptions{file_actions,..Default::default()},&mut||state.allow(true))?}
            else if request.pidls.is_empty(){menu.track_folder_background_with_interceptions(window,point,request.folder.as_deref().unwrap(),0,MenuInterceptions{file_actions,..Default::default()},&mut||state.allow(true))?}
            else if let Some(host)=&background_view{menu.track_selected_in_view_before(window,point,&request.pidls,&host.browser,
                MenuInterceptions{rename:request.rename,open:request.open,file_actions},&mut||state.allow(true))?}
            else{menu.track_selected_before(window,point,&request.pidls,0,MenuInterceptions{rename:request.rename,open:request.open,file_actions},&mut||state.allow(true))?};
        if outcome == MenuOutcome::CompressZip {
            state.allow(true)?;
            let paths = zsdesk_windows::shell::file_transfer::paths_from_pidls(&request.pidls)?;
            let path = zsdesk_windows::archive::compress_zip(&paths)?;
            state.status.created.push(zsdesk_windows::shell::new_menu::CreatedItem { path, rename:false });
            return Ok(MenuOutcome::ShellInvoked);
        }
        if matches!(outcome,MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled){
            pump(150);
            // ShellNew can open a modeless shortcut/template wizard. Its site
            // must remain alive until the wizard finishes its create callback.
            if creation.is_some() || is_background {while process_visible_windows(std::process::id(),window){pump(50);}}
            if let Some(creation)=&creation{state.status.created=creation.take();}
            let layout=background_view.as_ref().filter(|_|is_background).and_then(|host|zsdesk_windows::shell::capture_background_layout(&host.view));
            if layout!=initial_layout{
                state.status.layout_sort_changed=layout.as_ref().zip(initial_layout.as_ref()).is_some_and(|(after,before)|after.sort_columns!=before.sort_columns);
                state.status.layout=layout;
            }
        }
        Ok(outcome)
    })();

    #[cfg(test)]let _retirement_dialog=if state.request.probe.as_deref()==Some("retirement-dialog")&&result.is_ok(){
        unsafe extern "system" fn dialog_proc(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{unsafe{DefWindowProcW(hwnd,message,wp,lp)}}
        // Match the normal tracking path's first-show handoff before creating
        // a modeless extension window in the hidden worker process.
        unsafe{let _=ShowWindow(window,SW_SHOWNOACTIVATE);}
        let name=w!("ZSDesk.Test.PendingExtension");
        unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(dialog_proc),lpszClassName:name,..Default::default()});}
        let popup=Window(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW,name,name,WS_OVERLAPPEDWINDOW,20,20,240,100,None,None,Some(instance),None)}?);
        unsafe{let _=ShowWindow(popup.0,SW_SHOWNOACTIVATE);}
        let visible=unsafe{IsWindowVisible(popup.0)}.as_bool();let enumerable=process_visible_windows(std::process::id(),window);
        let mut bounds=windows::Win32::Foundation::RECT::default();let bounds_result=unsafe{GetWindowRect(popup.0,&mut bounds)};
        ensure!(visible&&enumerable,"controlled extension window is not visible/enumerable: visible={visible}, enumerable={enumerable}, bounds={bounds:?}, result={bounds_result:?}, hwnd={:?}",popup.0);
        Some(popup)
    }else{None};
    match result{
        Ok(outcome)=>state.status.outcome=Some(outcome),
        Err(error) if EventSet::signaled(events.dismiss)&&failure_code(&error)==windows::Win32::Foundation::E_ABORT=>state.status.outcome=Some(MenuOutcome::Dismissed),
        Err(error) if error.downcast_ref::<windows::core::Error>().is_some_and(|error|zsdesk_windows::shell::file_transfer::is_cancelled(error.code()))=>state.status.outcome=Some(MenuOutcome::Dismissed),
          Err(error)=>{state.status.error_hresult=Some(failure_code(&error).0);
            state.status.error_phase=Some(state.status.phase.clone());state.status.error=Some(format!("{error:#}"));},
    }
    #[cfg(test)]{state.status.probe_menu_open=watch.menu_open.get();state.status.probe_classic_loop=watch.classic_loop.get();state.status.probe_expanded_requested=watch.expanded_requested.get();state.status.probe_command_messages=unsafe{zs_menu_command_messages()}+watch.classic_commands.get();
        if state.request.probe.as_deref()==Some("modern-owner-copy"){state.status.probe_clipboard_after_invoke=isolation_tests::clipboard_metadata();}}
    let persistence_deadline=Instant::now()+Duration::from_secs(1);
    let persisted=loop{
        if !owns_clipboard(){break true;}
        match zsdesk_windows::shell::file_transfer::persist_owned_file_clipboard(window){
            Ok(persisted)=>break persisted,
            Err(error) if matches!(error.code().0 as u32,0x800401d0|0x80070005|0x800700aa)&&Instant::now()<persistence_deadline=>pump(25),
            Err(_)=>break false,
        }
    };
    let host_action=matches!(state.status.outcome,Some(MenuOutcome::Application(_)|MenuOutcome::Rename|MenuOutcome::Open|
        MenuOutcome::Explore|MenuOutcome::Copy|MenuOutcome::Cut|MenuOutcome::Paste|MenuOutcome::Delete{..}));
    if state.status.before_invoke&&!host_action{
        // Property sheets and similar command UI appear asynchronously; let
        // them become visible before deciding whether this process is idle.
        pump(150);
    }
    // A drawn request owns one short-lived isolation process. The existing
    // done path still pumps modeless dialogs and protects clipboard ownership.
    let reusable=(state.request.mode.prewarm() || !state.request.options.drawn)&&state.status.error.is_none()&&state.request.ordinal<MAX_SESSION_REQUESTS
        &&persisted&&!owns_clipboard()&&!process_visible_windows(std::process::id(),window)&&!EventSet::signaled(events.cancel);
    if reusable{
        // Release all selection/menu objects and their real owner before
        // advertising idle. COM itself remains alive for a later fresh menu.
        drop(owned_window);drop(watch);state.status.window=0;state.status.reusable=true;state.state("idle")?;
        return Ok(true);
    }
    state.state("done")?;
    // Already-granted commands, modeless dialogs and virtual-file providers
    // own their lifetime. They never enter the pool and are never killed here.
    while process_visible_windows(std::process::id(),window){pump(50);}
    if !persisted{while owns_clipboard(){pump(50);}}
    Ok(false)
}

// Shell jobs may retain the hosting STA after InvokeCommand returns. Keep the
// COM pump available for those explicit references without parking an idle menu.
struct ShellThreadRef{
    references:Box<std::sync::atomic::AtomicI32>,
    own:Option<windows::core::IUnknown>,
}
impl ShellThreadRef{
    fn install()->windows::core::Result<Option<Self>>{
        use windows::Win32::UI::Shell::{SHCreateThreadRef,SHGetThreadRef,SHSetThreadRef};
        if unsafe{SHGetThreadRef()}.is_ok(){return Ok(None);}
        let references=Box::new(std::sync::atomic::AtomicI32::new(0));
        let mut own=None;
        unsafe{SHCreateThreadRef(references.as_ptr(),&mut own)?;SHSetThreadRef(own.as_ref())?;}
        Ok(Some(Self{references,own}))
    }
}
impl Drop for ShellThreadRef{
    fn drop(&mut self){
        let _=unsafe{windows::Win32::UI::Shell::SHSetThreadRef(None::<&windows::core::IUnknown>)};
        drop(self.own.take());
        while self.references.load(Ordering::Acquire)>0{pump(25);}
    }
}
fn run_worker(path:&Path)->Result<()>{
    ensure!(path.is_absolute()&&path.file_name().is_some_and(|name|name=="request.json"),"菜单请求路径无效");
    ensure!(std::env::var_os("ZSDESK_MENU_WORKSPACE").map(PathBuf::from).as_deref()==path.parent(),"菜单工作目录不匹配");
    let session=std::env::var("ZSDESK_MENU_SESSION").context("菜单会话缺失")?;
    let mut request:Request=read_small(path,MAX_REQUEST)?;
    validate(&request)?;
    ensure!(session==request.session&&request.ordinal==1&&parent_alive(&request),"菜单请求归属无效");
    let parent=(request.parent,request.parent_created);let events=EventSet::open(&session)?;
    let deadline=Instant::now()+Duration::from_secs(5);
    loop{
        if EventSet::signaled(events.request){break;}
        ensure!(parent_alive(&request)&&!EventSet::signaled(events.cancel)&&Instant::now()<deadline,"菜单进程尚未建立资源边界");
        match wait_signal(&[events.request,events.cancel],50){Some(0)=>break,Some(1)=>anyhow::bail!("菜单会话已取消"),_=>pump(0)}
    }
    let fault_report_available=match zsdesk_windows::shell::menu::install_fault_report(&path.with_file_name("fault.json")){
        Ok(())=>true,Err(error)=>{eprintln!("无法启用扩展故障模块记录：{error}");false},
    };
    let com_started=Instant::now();unsafe{SetErrorMode(0x0001|0x0002);OleInitialize(None)?;}
    let mut com_us=micros(com_started.elapsed());
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let _thread_ref=if request.options.drawn{ShellThreadRef::install()?}else{None};
    struct MenuResources;impl Drop for MenuResources{fn drop(&mut self){
        zsdesk_windows::shell::menu::release_menu_thread_resources();
        // Reapers must finish removing the owned state workspaces before this
        // isolated menu host exits. No provider command is invoked by them.
        while zsdesk_windows::shell::pending_packaged_state_workers()>0 { pump(20); }
    }}let _menu_resources=MenuResources;
    let _background_cache=background_view::CacheLifetime;
    // An idle process may still host UI or clipboard data of a command it ran
    // for an earlier request (it is reusable while none is visible yet). Never
    // end those with the process.
    let linger=||{
        while process_visible_windows(std::process::id(),HWND::default()){pump(50);}
        while owns_clipboard(){pump(50);}
    };
    loop{
        let ordinal=request.ordinal;let token=request.token.clone();
        let owner=MenuOwner::capture(HWND(request.owner as *mut _));
        if !run_request(path,request, &events,com_us,fault_report_available)?{return Ok(());}com_us=0;
        let native=zsdesk_windows::shell::menu_diagnostics().runtime_initializations>0;
        let deadline=Instant::now()+IDLE_TIMEOUT;
        loop{
            let expired=if native{!owner.alive()}else{Instant::now()>=deadline};
            if EventSet::signaled(events.cancel)||expired||created(parent.0)!=Some(parent.1){linger();return Ok(());}
            match wait_signal(&[events.request,events.cancel],1000){
                Some(0)=>break,Some(1)=>{linger();return Ok(());},_=>pump(0),
            }
        }
        request=read_small(path,MAX_REQUEST)?;validate(&request)?;
        ensure!(request.session==session&&request.token!=token&&request.ordinal==ordinal+1&&(request.parent,request.parent_created)==parent&&parent_alive(&request),"复用菜单请求归属无效");
    }
}
fn owns_clipboard()->bool{
    // Extensions may publish a virtual-file provider from their own STA. The
    // process must remain alive for that provider too; only current-STA HDROP
    // snapshots qualify for bounded static conversion by file_transfer.
    unsafe{GetClipboardOwner()}.ok().is_some_and(|owner|{let mut process=0;unsafe{GetWindowThreadProcessId(owner,Some(&mut process));}process==std::process::id()})
}
fn system_input_indicator(hwnd:HWND,process:u32)->bool {
    // TSF creates these two visible helper windows on the menu STA's desktop.
    // They are system input UI, not extension dialogs; verify both class and
    // the owning system module before exempting them from lifetime checks.
    let mut class=[0u16;128];let count=unsafe{GetClassNameW(hwnd,&mut class)}.max(0)as usize;
    if !matches!(String::from_utf16_lossy(&class[..count]).as_str(),"UAC Input Indicator"|"UAC_InputIndicatorOverlayWnd"){return false;}
    let module=unsafe{GetClassLongPtrW(hwnd,GCLP_HMODULE)};
    if module==0{return false;}
    let mut name=[0u16;32768];
    let length=if process==std::process::id(){
        (unsafe{windows::Win32::System::LibraryLoader::GetModuleFileNameW(Some(windows::Win32::Foundation::HMODULE(module as *mut _)),&mut name)})as usize
    }else{
        use windows::Win32::System::Threading::{PROCESS_QUERY_INFORMATION,PROCESS_VM_READ};
        let Ok(handle)=(unsafe{OpenProcess(PROCESS_QUERY_INFORMATION|PROCESS_VM_READ,false,process)})else{return false;};
        #[link(name="kernel32")]unsafe extern "system"{fn K32GetModuleFileNameExW(process:HANDLE,module:usize,path:*mut u16,size:u32)->u32;}
        let count=unsafe{K32GetModuleFileNameExW(handle,module as usize,name.as_mut_ptr(),name.len()as u32)}as usize;
        let _=unsafe{CloseHandle(handle)};count
    };
    if length==0||length>=name.len(){return false;}
    let mut system=[0u16;32768];let count=unsafe{windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut system))}as usize;
    if count==0||count>=system.len(){return false;}
    let expected=PathBuf::from(String::from_utf16_lossy(&system[..count])).join("InputSwitch.dll");
    String::from_utf16_lossy(&name[..length]).eq_ignore_ascii_case(&expected.to_string_lossy())
}
fn process_visible_windows(process:u32,exclude:HWND)->bool{
    struct Find{process:u32,exclude:HWND,found:bool}let mut state=Find{process,exclude,found:false};
    unsafe extern "system" fn each(hwnd:HWND,data:LPARAM)->windows::core::BOOL{
        let state=unsafe{&mut*(data.0 as*mut Find)};let mut pid=0;unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
        if pid==state.process&&hwnd!=state.exclude&&unsafe{IsWindowVisible(hwnd)}.as_bool(){
            if system_input_indicator(hwnd,pid){return true.into();}
            let mut rect=windows::Win32::Foundation::RECT::default();
            if unsafe{GetWindowRect(hwnd,&mut rect)}.is_ok()&&rect.right>rect.left&&rect.bottom>rect.top{state.found=true;return false.into();}
        }true.into()
    }
    let _=unsafe{EnumWindows(Some(each),LPARAM(&mut state as*mut Find as isize))};state.found
}
pub fn run_worker_if_requested()->Option<Result<()>>{
    let mut args=std::env::args_os().skip(1);if args.next()?.to_str()!=Some("--context-menu-worker"){return None;}
    Some((||{let _lifetime=crate::ApplicationLifetime::new();let path=args.next().context("缺少菜单请求")?;ensure!(args.next().is_none(),"菜单参数无效");run_worker(Path::new(&path))})())
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn atomic_publication_survives_a_short_delete_sharing_lock()->Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        let root=std::env::temp_dir().join(format!("zsdesk-menu-write-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root)?;let path=root.join("status.json");write(&path,&1u32)?;
        let lock=fs::OpenOptions::new().read(true).share_mode(1).open(&path)?;
        let release=std::thread::spawn(move||{std::thread::sleep(Duration::from_millis(40));drop(lock);});
        let result=write(&path,&2u32);release.join().unwrap();result?;
        ensure!(read_small::<u32>(&path,32)?==2,"atomic replacement lost the new packet");
        fs::remove_file(&path)?;fs::remove_dir(&root)?;Ok(())
    }
    #[test]fn menu_protocol_rejects_truncated_or_oversized_identities(){
        assert!(valid_pidl(&[0,0]));assert!(valid_pidl(&[3,0,1,0,0]));
        for invalid in [vec![],vec![1,0,0,0],vec![4,0,1],vec![0,0,9],vec![0;65537]]{assert!(!valid_pidl(&invalid));}
    }
    #[test]fn successful_requests_preserve_the_last_crash()->Result<()>{
        let root=std::env::temp_dir().join(format!("zsdesk-crash-record-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root)?;
        let crash=serde_json::json!({"worker_exit_hex":"0xC0000005","phase":"tracking"});
        ensure!(save_crash(&root,Some(0xc0000005u32 as i32),&crash));
        for exit in [None,Some(0)]{ensure!(!save_crash(&root,exit,&serde_json::json!({"phase":"done"})));}
        let path=root.join("menu-last-crash.json");
        ensure!(read_small::<serde_json::Value>(&path,1024)?==crash);
        fs::remove_file(path)?;fs::remove_dir(root)?;Ok(())
    }
    #[test]fn menu_protocol_rejects_unbounded_or_path_like_sessions(){
        let mut request=Request{english:None,token:"request-1".into(),session:"session-1".into(),ordinal:1,parent:1,parent_created:1,owner:1,
            point:[0,0],pidls:vec![vec![0,0]],folder:None,options:Default::default(),rename:false,open:false,file_actions:false,mode:Mode::More,extended:false,layout:None,view_context:None,probe:None};
        assert!(validate(&request).is_ok());
        for value in ["",r"..\escape","bad/session","bad\0session"]{request.session=value.into();assert!(validate(&request).is_err());}
        request.session="session-1".into();for ordinal in [0,MAX_SESSION_REQUESTS+1]{request.ordinal=ordinal;assert!(validate(&request).is_err());}
    }
    #[test]
    fn background_menu_accepts_serialized_details_columns_and_grouping() -> Result<()> {
        use zsdesk_core::model::explorer::{ViewColumn, ViewGrouping, ViewPropertyKey, ViewSortColumn};
        let key = |property_id| ViewPropertyKey { format_id: "B725F130-47EF-101A-A5F1-02608C9EEBAC".into(), property_id };
        let layout = ShellViewState {
            mode: 4, icon_size: 16, column_dpi: 96,
            columns: [10, 14, 4, 12].into_iter().map(|id| ViewColumn { key: key(id), width: 144 }).collect(),
            sort_columns: vec![ViewSortColumn { key: key(10), ascending: true }],
            grouping: Some(ViewGrouping { key: Some(key(14)), ascending: false }),
            ..Default::default()
        };
        let mut request = Request { english: None, token: "request-1".into(), session: "session-1".into(), ordinal: 1,
            parent: 1, parent_created: 1, owner: 1, point: [0,0], pidls: vec![vec![0,0]], folder: None,
            options: Default::default(), rename: false, open: false, file_actions: false, mode: Mode::Background,
            extended: false, layout: Some(layout.clone()), view_context: None, probe: None };
        for mode in [Mode::Background, Mode::New, Mode::CatalogBackground] {
            request.mode = mode;
            let decoded: Request = serde_json::from_slice(&serde_json::to_vec(&request)?)?;
            validate(&decoded)?;
            assert_eq!(decoded.layout.as_ref(), Some(&layout));
        }
        for mode in [Mode::More, Mode::CatalogSelection] {
            request.mode = mode;
            assert!(validate(&request).is_err());
            request.layout.as_mut().unwrap().columns.clear();
            validate(&request)?;
            request.layout = Some(layout.clone());
        }
        request.mode = Mode::Background;
        // Profiles and older request senders can omit DPI.
        let mut legacy = serde_json::to_value(&request)?;
        legacy["layout"].as_object_mut().unwrap().remove("column_dpi");
        validate(&serde_json::from_value(legacy)?)?;
        for change in [
            (|s: &mut ShellViewState| s.columns.resize(65, s.columns[0].clone())) as fn(&mut ShellViewState),
            |s| s.columns[0].width = 0, |s| s.columns[0].width = 16_385,
            |s| s.columns[0].key.format_id = "invalid".into(), |s| s.column_dpi = u32::MAX,
            |s| s.selected.push(vec![0,0]), |s| s.focused = Some(vec![0,0]), |s| s.first_visible = Some(vec![0,0]),
        ] {
            let mut invalid = request.clone();
            change(invalid.layout.as_mut().unwrap());
            assert!(validate(&invalid).is_err());
        }
        Ok(())
    }
    #[test]fn filesystem_action_interception_is_opt_in_on_the_wire()->Result<()>{
        let request=Request{english:None,token:"request-1".into(),session:"session-1".into(),ordinal:1,parent:1,parent_created:1,owner:1,
            point:[0,0],pidls:Vec::new(),folder:Some(PathBuf::from(r"E:\destination")),options:Default::default(),rename:false,open:false,
            file_actions:true,mode:Mode::Paste,extended:false,layout:None,view_context:None,probe:None};
        ensure!(validate(&request).is_ok(),"background paste target rejected");
        let mut json=serde_json::to_value(&request)?;let restored:Request=serde_json::from_value(json.clone())?;assert!(restored.file_actions);
        json.as_object_mut().unwrap().remove("file_actions");let legacy:Request=serde_json::from_value(json)?;assert!(!legacy.file_actions);
        let mut invalid=legacy;invalid.folder=None;assert!(validate(&invalid).is_err());
        for permanent in [false,true]{let value=MenuOutcome::Delete{permanent};assert_eq!(serde_json::from_str::<MenuOutcome>(&serde_json::to_string(&value)?)?,value);}
        Ok(())
    }
    #[test]fn menu_events_wake_once_but_cancellation_stays_latched()->Result<()>{
        let events=EventSet::create(&format!("test-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed)))?;
        EventSet::signal(events.status);assert_eq!(wait_signal(&[events.status],0),Some(0));assert!(!EventSet::signaled(events.status));
        EventSet::signal(events.dismiss);assert!(EventSet::signaled(events.dismiss));assert!(!EventSet::signaled(events.cancel));
        EventSet::signal(events.cancel);assert!(EventSet::signaled(events.cancel));assert!(EventSet::signaled(events.cancel));
        events.reset();assert!(!EventSet::signaled(events.cancel)&&!EventSet::signaled(events.dismiss));Ok(())
    }
    #[test]fn menu_diagnostics_omit_user_paths_but_keep_error_codes(){
        let value=redacted_error("0xC0000005 failed C:\\Users\\Example\\private file.txt\nmodule WPS.dll\nnetwork \\\\server\\private");
        assert!(value.contains("0xC0000005")&&value.contains("WPS.dll"));
        assert!(!value.contains("Example")&&!value.contains("server")&&!value.contains("private"));
    }
    #[test]fn reused_worker_diagnostics_keep_the_latest_failure_without_partial_paths()->Result<()>{
        let root=std::env::temp_dir().join(format!("zsdesk-stderr-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&root)?;
        struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_file(self.0.join("worker.stderr.log"));let _=fs::remove_dir(&self.0);}}let _fixture=Fixture(root.clone());
        let mut text="obsolete request\n".repeat(500);text.push_str(&format!("C:\\Users\\Private\\{}\n","hidden".repeat(800)));
        text.push_str("latest failure 0x80004001\nsource C:\\Users\\Example\\secret.txt\n");
        fs::write(root.join("worker.stderr.log"),text)?;
        let raw=worker_stderr(&root);let visible=redacted_error(&raw);
        assert!(visible.contains("latest failure 0x80004001"));assert!(!visible.contains("obsolete")&&!visible.contains("hidden")&&!visible.contains("secret")&&!visible.contains("Example"));
        fs::write(root.join("worker.stderr.log"),"unfinished".repeat(900))?;assert!(worker_stderr(&root).is_empty());Ok(())
    }
    #[test]fn menu_failure_keeps_native_hresult_and_distinguishes_invocation(){
        let failed=MenuFailure{phase:"invoking".into(),code:Some(0x80070005u32 as i32),detail:"权限不足 (0x80070005)".into(),invoked:true};
        let error:anyhow::Error=failed.into();assert_eq!(failure_code(&error).0 as u32,0x80070005);
        assert!(failure_message(&error).starts_with("右键菜单命令执行失败"));assert!(!failure_message(&error).contains("加载失败"));
        let binding:anyhow::Error=MenuFailure{phase:"binding".into(),code:Some(0x80004005u32 as i32),detail:"namespace unavailable".into(),invoked:false}.into();
        assert!(failure_message(&binding).starts_with("无法获取当前位置的系统菜单"));
    }
    #[test]fn filesystem_failures_keep_specific_codes_instead_of_generic_failure(){
        for (native,expected) in [(5,0x80070005u32),(2,0x80070002),(0xc0000005u32 as i32,0xc0000005)]{
            let error=anyhow::Error::new(std::io::Error::from_raw_os_error(native)).context("operation failed");
            assert_eq!(failure_code(&error).0 as u32,expected);
        }
        assert_eq!(failure_code(&anyhow::anyhow!("unspecified provider failure")).0 as u32,0x80004005);
    }
    #[test]#[ignore="Internal subprocess entry for menu isolation regression"]
    fn native_extension_worker_entry()->Result<()>{
        let _state_entry=zsdesk_windows::shell::packaged_state_worker_test_entry(&[
            "context_menu_process::tests::native_packaged_state_worker_entry".into(),"--exact".into(),"--ignored".into(),"--nocapture".into()])?;
        #[link(name="user32")]unsafe extern "system"{fn OpenDesktopW(name:*const u16,flags:u32,inherit:i32,access:u32)->*mut core::ffi::c_void;fn SetThreadDesktop(desktop:*mut core::ffi::c_void)->i32;}
        if let Ok(expected)=std::env::var("ZSDESK_MENU_TEST_STATION"){
            ensure!(isolation_tests::current_station_name()?==expected,"worker did not connect to the private test window station");
        }
        if let Ok(name)=std::env::var("ZSDESK_MENU_TEST_DESKTOP"){
            let desktop=unsafe{OpenDesktopW(wide(&name).as_ptr(),0,0,0x000f00ff)};ensure!(!desktop.is_null()&&unsafe{SetThreadDesktop(desktop)}!=0,"private worker desktop unavailable");
        }
        let root=PathBuf::from(std::env::var_os("ZSDESK_MENU_WORKSPACE").context("workspace")?);run_worker(&root.join("request.json"))
    }
    #[test]#[ignore="Internal subprocess entry for packaged command state evaluation"]
    fn native_packaged_state_worker_entry()->Result<()>{Ok(zsdesk_windows::shell::run_packaged_state_test_worker()?)}
}
#[cfg(test)]#[path="context_menu_process_tests.rs"]pub(crate) mod isolation_tests;
#[cfg(test)]#[path="context_menu_namespace_tests.rs"]mod namespace_tests;
#[cfg(test)]#[path="context_menu_runtime_tests.rs"]mod runtime_tests;
