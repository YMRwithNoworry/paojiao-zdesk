//! Bounded process isolation for Shell extensions. Each request owns a fresh
//! immutable selection/menu; an idle STA may serve a later verified request.
use std::{fs,io::Read,path::{Path,PathBuf},process::{Child,Command,Stdio},sync::{Mutex,atomic::{AtomicU64,Ordering}},time::{Duration,Instant}};
#[cfg(test)]use std::thread;
use std::os::windows::{process::CommandExt,fs::MetadataExt,io::AsRawHandle};
use anyhow::{Context,Result,ensure};
use serde::{Deserialize,Serialize};
use windows::{core::{PCWSTR,w},Win32::{Foundation::{HWND,HINSTANCE,LPARAM,LRESULT,WPARAM,POINT,HANDLE,CloseHandle,FILETIME},
    System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize},Threading::{OpenProcess,GetProcessTimes,PROCESS_QUERY_LIMITED_INFORMATION},DataExchange::GetClipboardOwner,JobObjects::*},
    UI::WindowsAndMessaging::*}};
use zsdesk_windows::shell::menu::{ContextMenuOptions,MenuInterceptions,MenuOutcome,ShellMenu,forward_menu_message};
use zsdesk_core::model::explorer::ShellViewState;
#[path="context_menu_background.rs"]mod background_view;

const MAX_REQUEST:u64=16*1024*1024;
const MAX_ITEMS:usize=8192;
const WORKER_MEMORY_LIMIT:usize=512*1024*1024;
static SERIAL:AtomicU64=AtomicU64::new(1);
static RETIRING:Mutex<Vec<RetiredChild>>=Mutex::new(Vec::new());
const IDLE_TIMEOUT:Duration=Duration::from_secs(60);
const MAX_SESSION_REQUESTS:u32=32;
const MAX_IDLE_WORKERS:usize=4;
static IDLE:Mutex<Vec<IdleWorker>>=Mutex::new(Vec::new());
static DIAGNOSTICS_LOCK:Mutex<()>=Mutex::new(());
static PREPARING:std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
static REAPING:std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
struct Origin {thread:u32,desktop:usize,started:Instant,counter:u64,layout:Option<ShellViewState>,#[cfg(test)]probe:Option<String>}
impl Origin {fn capture()->Self{let thread=unsafe{windows::Win32::System::Threading::GetCurrentThreadId()};Self{
    thread,desktop:unsafe{GetThreadDesktop(thread)}as usize,started:Instant::now(),counter:monotonic_us(),layout:None,
    #[cfg(test)]probe:std::env::var("ZSDESK_MENU_TEST_PROBE").ok()}}}

struct EventSet { request:usize,status:usize,decision:usize,cancel:usize }
impl EventSet {
    fn names(session:&str)->[String;4] { ["request","status","decision","cancel"].map(|part|format!("Local\\ZSDesk.Menu.{session}.{part}")) }
    fn create(session:&str)->Result<Self> {
        use windows::Win32::System::Threading::CreateEventW;
        let mut handles=Vec::new();
        for (index,name) in Self::names(session).iter().enumerate() {
            match unsafe{CreateEventW(None,index==3,false,PCWSTR(wide(name).as_ptr()))} {
                Ok(handle)=>handles.push(handle.0 as usize),
                Err(error)=>{for handle in handles{let _=unsafe{CloseHandle(HANDLE(handle as*mut _))};}return Err(error.into());}
            }
        }
        Ok(Self{request:handles[0],status:handles[1],decision:handles[2],cancel:handles[3]})
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
        Ok(Self{request:handles[0],status:handles[1],decision:handles[2],cancel:handles[3]})
    }
    fn signal(handle:usize){let _=unsafe{windows::Win32::System::Threading::SetEvent(HANDLE(handle as*mut _))};}
    fn signaled(handle:usize)->bool{unsafe{windows::Win32::System::Threading::WaitForSingleObject(HANDLE(handle as*mut _),0)}.0==0}
    fn reset(&self){for handle in [self.request,self.status,self.decision,self.cancel]{let _=unsafe{windows::Win32::System::Threading::ResetEvent(HANDLE(handle as*mut _))};}}
}
impl Drop for EventSet{fn drop(&mut self){for handle in [self.request,self.status,self.decision,self.cancel]{let _=unsafe{CloseHandle(HANDLE(handle as*mut _))};}}}

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

struct IdleWorker {
    child:Option<Child>,job:Option<usize>,workspace:PathBuf,session:String,token:String,events:Option<EventSet>,
    count:u32,created:u64,last_used:Instant,desktop:usize,thread:u32,
    /// A command already ran in this process. Its dialogs, clipboard data or
    /// file operations own the process lifetime: it exits by itself and is
    /// never terminated on retirement.
    committed:bool,
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
            std::thread::sleep(Duration::from_millis(100));
        }
        REAPING.store(false,Ordering::Release);
        if !IDLE.lock().unwrap_or_else(|e|e.into_inner()).is_empty()||!RETIRING.lock().unwrap_or_else(|e|e.into_inner()).is_empty(){start_reaper();}
    }).is_err(){REAPING.store(false,Ordering::Release);}
}
pub fn shutdown_idle_worker(){let idle=std::mem::take(&mut*IDLE.lock().unwrap_or_else(|error|error.into_inner()));drop(idle);}
pub fn shutdown_idle_worker_for_thread(){
    let thread=unsafe{windows::Win32::System::Threading::GetCurrentThreadId()};
    let idle={let mut pool=IDLE.lock().unwrap_or_else(|error|error.into_inner());pool.iter().position(|idle|idle.thread==thread).map(|index|pool.swap_remove(index))};drop(idle);
}
pub fn idle_worker_diagnostics()->serde_json::Value{
    let idle=IDLE.lock().unwrap_or_else(|error|error.into_inner());
    serde_json::Value::Array(idle.iter().map(|idle|serde_json::json!({"pid":idle.child.as_ref().map(Child::id),"created":idle.created,
        "identity_verified":idle.child.as_ref().is_some_and(|child|created(child.id())==Some(idle.created)),
        "owner_thread":idle.thread,"idle_milliseconds":micros(idle.last_used.elapsed())/1000,
        "idle_limit_milliseconds":IDLE_TIMEOUT.as_millis()as u64,"requests":idle.count,"request_limit":MAX_SESSION_REQUESTS})).collect())
}
fn retire_expired_idle(){
    let retired={let mut idle=IDLE.lock().unwrap_or_else(|error|error.into_inner());let mut retired=Vec::new();let mut index=0;
        while index<idle.len(){let expired=idle[index].last_used.elapsed()>=IDLE_TIMEOUT||idle[index].child.as_mut().is_none_or(|child|child.try_wait().ok().flatten().is_some());
            if expired{retired.push(idle.swap_remove(index));}else{index+=1;}}retired};drop(retired);
}
pub fn pending_workers()->usize{
    collect_workers()+PREPARING.load(Ordering::Acquire)+usize::from(REAPING.load(Ordering::Acquire))
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
        "previous":read_small::<serde_json::Value>(&root.join("menu-diagnostics.previous.json"),128*1024).ok(),"idle_workers":idle_worker_diagnostics()})
}
fn retain_retired_fault(worker:&RetiredChild,exit:Option<i32>){
    let _guard=DIAGNOSTICS_LOCK.lock().unwrap_or_else(|error|error.into_inner());
    let Some(root)=worker.workspace.parent()else{return;};let current=root.join("menu-diagnostics.current.json");
    let old=read_small::<serde_json::Value>(&current,128*1024).ok();
    if old.as_ref().is_some_and(|old|old["worker_pid"].as_u64()==Some(u64::from(worker.child.id()))&&old["worker_exit_code"].as_i64()==exit.map(i64::from)){return;}
    let status=read_small::<Status>(&worker.workspace.join("status.json"),65536).ok();
    let value=serde_json::json!({"schema":"ZSDesk.MenuDiagnostics.v1","timestamp_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|time|time.as_millis()as u64),
        "parent_pid":std::process::id(),"worker_pid":worker.child.id(),"worker_created":worker.identity,
        "phase":status.as_ref().map(|status|status.phase.as_str()).unwrap_or("retiring"),"worker_exit_code":exit,"worker_exit_hex":exit.map(|code|format!("0x{:08X}",code as u32)),
        "committed":worker.committed,"timing":status.as_ref().map(|status|&status.timing),"fault":read_small::<serde_json::Value>(&worker.workspace.join("fault.json"),16384).ok(),
        "error":"系统扩展菜单进程在请求完成后异常退出","stderr":redacted_error(&worker_stderr(&worker.workspace))});
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
pub enum Mode{More,Properties,Open,Background,New}
#[derive(Clone,Serialize,Deserialize)]
struct Request{
    token:String,parent:u32,parent_created:u64,owner:isize,point:[i32;2],pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,
    #[serde(default)]session:String,
    #[serde(default)]ordinal:u32,
    options:ContextMenuOptions,rename:bool,open:bool,mode:Mode,extended:bool,
    #[serde(default)]layout:Option<ShellViewState>,
    #[cfg(test)]probe:Option<String>,
}
#[derive(Clone,Debug,Default,Serialize,Deserialize)]
pub struct MenuTimings {
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
struct Status{token:String,pid:u32,window:isize,phase:String,validation:u64,before_invoke:bool,outcome:Option<MenuOutcome>,error:Option<String>,bind_microseconds:u64,populate_microseconds:u64,
    #[serde(default)]created:Vec<zsdesk_windows::shell::new_menu::CreatedItem>,
    #[serde(default)]layout:Option<ShellViewState>,
    #[serde(default)]reusable:bool,
    #[serde(default)]timing:MenuTimings,
    #[serde(default)]native_menu:serde_json::Value,
    #[cfg(test)]#[serde(default)]probe_menu_open:bool,
    #[cfg(test)]#[serde(default)]probe_command_messages:u64,
}
#[derive(Serialize,Deserialize)]
struct Decision{token:String,validation:u64,permit:bool}
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
    let path=workspace.join("worker.stderr.log");
    let Ok(metadata)=fs::symlink_metadata(&path)else{return String::new();};
    if !metadata.is_file()||metadata.file_attributes()&0x400!=0{return String::new();}
    let mut bytes=Vec::new();
    if fs::File::open(path).and_then(|file|file.take(4096).read_to_end(&mut bytes)).is_err(){return String::new();}
    String::from_utf8_lossy(&bytes).trim().to_owned()
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
    ensure!(!matches!(request.mode,Mode::Background|Mode::New)||request.pidls.len()==1,"背景菜单需要一个文件夹标识");
    ensure!(request.folder.as_ref().is_none_or(|folder|folder.is_absolute()),"菜单目录必须是绝对路径");
    ensure!(request.options.hidden_verbs.len()<=128&&request.options.hidden_verbs.iter().all(|verb|verb.len()<=128&&!verb.contains('\0')),"菜单设置无效");
    if let Some(layout)=&request.layout {
        ensure!(matches!(request.mode,Mode::Background|Mode::New)&&(1..=8).contains(&layout.mode)&&(8..=512).contains(&layout.icon_size)
            &&layout.sort_columns.len()<=64&&layout.columns.is_empty()&&layout.selected.is_empty()&&layout.focused.is_none()&&layout.first_visible.is_none(),"背景视图设置无效");
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
    mode:Mode,item_count:usize,owner_class:String,diagnostic_written:bool,
}
impl Client{
    pub fn spawn(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode)->Result<Self>{
        Self::spawn_extended(owner,point,pidls,folder,options,intercept,mode,false)
    }
    pub fn spawn_extended(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool)->Result<Self>{
        Self::spawn_at(owner,point,pidls,folder,options,intercept,mode,extended,Origin::capture())
    }
    fn spawn_at(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool,origin:Origin)->Result<Self>{
        let started=origin.started;let started_counter=origin.counter;
        let root=diagnostics_directory();ensure!(root.is_absolute(),"菜单缓存目录必须是绝对路径");fs::create_dir_all(&root)?;
        let parent_created=created(std::process::id()).context("无法核对菜单父进程")?;
        retire_expired_idle();
        let thread=origin.thread;let desktop=origin.desktop;
        let token=format!("{}-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
        let cached={let mut pool=IDLE.lock().unwrap_or_else(|error|error.into_inner());pool.iter().position(|idle|idle.thread==thread).map(|index|pool.swap_remove(index))};
        let reusable=cached.filter(|idle|idle.desktop==desktop&&idle.workspace.parent()==Some(root.as_path())&&idle.count<MAX_SESSION_REQUESTS
            &&idle.last_used.elapsed()<IDLE_TIMEOUT.saturating_sub(Duration::from_secs(2))
            &&idle.child.as_ref().is_some_and(|child|created(child.id())==Some(idle.created))
            &&read_small::<Status>(&idle.workspace.join("status.json"),65536).is_ok_and(|status|
                status.phase=="idle"&&status.reusable&&status.token==idle.token&&idle.child.as_ref().is_some_and(|child|status.pid==child.id())));
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
        let request=Request{token:token.clone(),session:session.clone(),ordinal,parent:std::process::id(),parent_created,owner:owner.0 as isize,
            point:[point.x,point.y],pidls,folder,options,rename:intercept.rename,open:intercept.open,mode,extended,layout:origin.layout,
            #[cfg(test)]probe:origin.probe,};
        let write_started=Instant::now();
        if let Err(error)=validate(&request).and_then(|_|write(&workspace.join("request.json"),&request)){
            if let Some(child)=child.take(){EventSet::signal(events.cancel);EventSet::signal(events.request);retire_child(child,job.take(),workspace.clone(),child_created,inherited_commit);}else{cleanup(&workspace);}return Err(error);
        }
        let mut timing=MenuTimings{reused_process:reused,request_write_microseconds:micros(write_started.elapsed()),..Default::default()};
        let identity=if !reused{
            let stderr=match fs::File::create(workspace.join("worker.stderr.log")){Ok(file)=>file,Err(error)=>{cleanup(&workspace);return Err(error.into());}};
            let mut command=Command::new(std::env::current_exe()?);
            #[cfg(not(test))]command.arg("--context-menu-worker").arg(workspace.join("request.json"));
            #[cfg(test)]command.arg("context_menu_process::tests::native_extension_worker_entry").args(["--ignored","--exact","--nocapture"]);
            command.env("ZSDESK_MENU_WORKSPACE",&workspace).env("ZSDESK_MENU_SESSION",&session)
                .creation_flags(0x08000000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
            let spawn_started=Instant::now();let mut spawned=match command.spawn(){Ok(child)=>child,Err(error)=>{cleanup(&workspace);return Err(error.into());}};
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
        if let Some(child)=&child{let _=unsafe{AllowSetForegroundWindow(child.id())};}
        Ok(Self{child,job,workspace,token,session,events:Some(events),child_created:identity,ordinal,thread,desktop,started,started_counter,inherited_commit,
            last_validation:0,committed:false,complete:false,status_dirty:true,last_status:None,timing,mode,item_count,owner_class,diagnostic_written:false})
    }
    pub fn pid(&self)->Option<u32>{self.child.as_ref().map(Child::id)}
    pub fn timings(&self)->Option<(u64,u64)>{self.last_status.as_ref().map(|status|(status.bind_microseconds,status.populate_microseconds))}
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
        if let Ok(status)=read_small::<Status>(&self.workspace.join("status.json"),65536){
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
    fn record(&mut self,exit:Option<i32>,error:Option<&str>){
        if self.diagnostic_written{return;}self.diagnostic_written=true;
        let _guard=DIAGNOSTICS_LOCK.lock().unwrap_or_else(|error|error.into_inner());
        let root=self.workspace.parent().unwrap_or(Path::new("."));
        let value=serde_json::json!({"schema":"ZSDesk.MenuDiagnostics.v1","timestamp_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|time|time.as_millis()as u64),
            "parent_pid":std::process::id(),"worker_pid":self.pid(),"worker_created":self.child_created,"owner_class":self.owner_class,"mode":self.mode,"selection_count":self.item_count,
            "phase":self.last_status.as_ref().map(|status|status.phase.as_str()).unwrap_or("launch"),"worker_exit_code":exit,
            "worker_exit_hex":exit.map(|code|format!("0x{:08X}",code as u32)),"committed":self.committed,
            "bind_microseconds":self.last_status.as_ref().map(|status|status.bind_microseconds),"populate_microseconds":self.last_status.as_ref().map(|status|status.populate_microseconds),
            "timing":self.timing,"fault":read_small::<serde_json::Value>(&self.workspace.join("fault.json"),16384).ok(),
            "error":error.map(redacted_error),"stderr":redacted_error(&worker_stderr(&self.workspace))});
        let current=root.join("menu-diagnostics.current.json");let previous=root.join("menu-diagnostics.previous.json");
        if let Ok(old)=read_small::<serde_json::Value>(&current,128*1024){let _=write(&previous,&old);}
        let _=write(&current,&value);
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
                return Some(match status.error{Some(error)=>Err(anyhow::anyhow!(error)),None=>Ok(status.outcome.unwrap_or(MenuOutcome::Dismissed))});
            }
        }
        if let Some(exit)=self.child.as_mut().and_then(|child|child.try_wait().ok().flatten()){
            self.read_status();self.complete=true;
            let error=if !exit.success(){Some(format!("系统扩展菜单异常退出（{exit}，0x{:08X}）；文件管理窗口可继续使用，诊断已保留",exit.code().unwrap_or_default()as u32))}
                else if let Some(status)=self.last_status.as_ref().filter(|status|matches!(status.phase.as_str(),"done"|"idle")){status.error.clone()}
                else{Some("系统扩展菜单未返回完整结果，请重试".into())};
            self.record(exit.code(),error.as_deref());
            return Some(match error{Some(error)=>Err(anyhow::anyhow!(error)),None=>Ok(self.last_status.as_ref().and_then(|status|status.outcome).unwrap_or(MenuOutcome::Dismissed))});
        }
        let showing=self.timing.parent_to_menu_microseconds.is_some();
        if !self.committed&&!showing&&self.started.elapsed()>Duration::from_secs(30){
            self.complete=true;self.cancel();let message="系统扩展菜单加载超时；文件管理窗口仍可使用，诊断已保留";
            self.record(None,Some(message));return Some(Err(anyhow::anyhow!(message)));
        }
        None
    }
    pub fn cancel(&mut self){if let Some(events)=&self.events{EventSet::signal(events.cancel);EventSet::signal(events.request);}}
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
            events:self.events.take(),count:self.ordinal,created:self.child_created,last_used:Instant::now(),desktop:self.desktop,thread:self.thread,committed};
        let retired={let mut pool=IDLE.lock().unwrap_or_else(|error|error.into_inner());let old=pool.iter().position(|worker|worker.thread==self.thread).map(|index|pool.swap_remove(index));
            let evicted=if pool.len()>=MAX_IDLE_WORKERS{Some(pool.remove(0))}else{None};pool.push(idle);(old,evicted)};drop(retired);start_reaper();return;
    }
    self.cancel();
    if let Some(child)=self.child.take(){retire_child(child,self.job.take(),self.workspace.clone(),self.child_created,committed);}
}}

enum AsyncEvent {Validate{before:bool,answer:std::sync::mpsc::SyncSender<bool>},Finished(Result<MenuOutcome>,Option<ShellViewState>,Vec<zsdesk_windows::shell::new_menu::CreatedItem>)}
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
    created:Vec<zsdesk_windows::shell::new_menu::CreatedItem>,
    events:std::sync::mpsc::Receiver<AsyncEvent>,
    cancel:std::sync::Arc<std::sync::atomic::AtomicBool>,
    pid:std::sync::Arc<std::sync::atomic::AtomicU32>,complete:bool,
    layout:Option<ShellViewState>,
}
impl AsyncClient {
    pub fn spawn(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode)->Result<Self>{
        Self::spawn_extended(owner,point,pidls,folder,options,intercept,mode,false)
    }
    pub fn spawn_extended(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool)->Result<Self>{
        Self::spawn_with_layout(owner,point,pidls,folder,options,intercept,mode,extended,None)
    }
    pub fn spawn_with_layout(owner:HWND,point:POINT,pidls:Vec<Vec<u8>>,folder:Option<PathBuf>,options:ContextMenuOptions,intercept:MenuInterceptions,mode:Mode,extended:bool,layout:Option<ShellViewState>)->Result<Self>{
        use std::sync::{Arc,atomic::{AtomicBool,AtomicU32},mpsc};
        ensure!(PREPARING.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<4).then_some(n+1)).is_ok(),"菜单正在处理其他请求，请稍后重试");
        struct Active;impl Drop for Active{fn drop(&mut self){PREPARING.fetch_sub(1,Ordering::AcqRel);}}
        let active=Active;
        let mut origin=Origin::capture();origin.layout=layout;let owner=owner.0 as isize;let coordinates=(point.x,point.y);
        let (sender,events)=mpsc::sync_channel(4);let cancel=Arc::new(AtomicBool::new(false));let pid=Arc::new(AtomicU32::new(0));
        let stop=cancel.clone();let process=pid.clone();
        std::thread::Builder::new().name("zsdesk-menu-protocol".into()).spawn(move||{
            let _active=active;
            if stop.load(Ordering::Acquire){return;}
            let client=Client::spawn_at(HWND(owner as *mut _),POINT{x:coordinates.0,y:coordinates.1},pidls,folder,options,intercept,mode,extended,origin);
            let mut client=match client{Ok(client)=>client,Err(error)=>{let _=sender.try_send(AsyncEvent::Finished(Err(error),None,Vec::new()));notify_owner(owner);return;}};
            process.store(client.pid().unwrap_or(0),Ordering::Release);
            loop {
                if stop.load(Ordering::Acquire){client.cancel();break;}
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
                if let Some(result)=result {let layout=client.last_status.as_ref().and_then(|status|status.layout.clone());let created=client.last_status.as_ref().map(|status|status.created.clone()).unwrap_or_default();let _=sender.try_send(AsyncEvent::Finished(result,layout,created));notify_owner(owner);break;}
                client.wait_for_progress(20);
            }
        }).context("无法启动菜单通信线程")?;
        Ok(Self{events,cancel,pid,complete:false,layout:None,created:Vec::new()})
    }
    pub fn pid(&self)->Option<u32>{let pid=self.pid.load(Ordering::Acquire);(pid!=0).then_some(pid)}
    pub fn take_layout(&mut self)->Option<ShellViewState>{self.layout.take()}
    pub fn take_created(&mut self)->Vec<zsdesk_windows::shell::new_menu::CreatedItem>{std::mem::take(&mut self.created)}
    pub fn poll(&mut self,before:&mut dyn FnMut(bool)->bool)->Option<Result<MenuOutcome>> {
        if self.complete{return None;}
        for _ in 0..4 {match self.events.try_recv() {
            Ok(AsyncEvent::Validate{before:invoke,answer})=>{let permit=!self.cancel.load(Ordering::Acquire)&&before(invoke);let _=answer.try_send(permit);},
            Ok(AsyncEvent::Finished(result,layout,created))=>{self.complete=true;self.layout=layout;self.created=created;return Some(result);},
            Err(std::sync::mpsc::TryRecvError::Empty)=>return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected)=>{self.complete=true;return Some(Err(anyhow::anyhow!("菜单通信已结束，请重新打开菜单")));},
        }}None
    }
}
impl Drop for AsyncClient {fn drop(&mut self){self.cancel.store(true,Ordering::Release);}}

struct Worker<'a>{request:Request,path:PathBuf,status:Status,events:&'a EventSet}
struct Watch{
    request:Request,path:PathBuf,status_event:usize,cancel_event:usize,worker_started:u64,
    menu_open:std::cell::Cell<bool>,
    tracking_started:std::cell::Cell<u64>,
    #[cfg(test)]command:std::cell::Cell<Option<u32>>,
}
extern "C"{fn zs_menu_is_open(owner:HWND)->i32;}
#[cfg(test)]extern "C"{fn zs_menu_command_messages()->u64;}
impl Worker<'_>{
    fn state(&mut self,phase:&str)->Result<()>{
        self.status.phase=phase.into();self.status.native_menu=serde_json::to_value(zsdesk_windows::shell::menu_diagnostics())?;
        write(&self.path.with_file_name("status.json"),&self.status)?;EventSet::signal(self.events.status);Ok(())
    }
    fn allow(&mut self,before_invoke:bool)->windows::core::Result<()>{
        let started=Instant::now();
        let result=(||->Result<()>{
            ensure!(parent_alive(&self.request)&&!EventSet::signaled(self.events.cancel),"菜单已取消");
            self.status.validation+=1;self.status.before_invoke=before_invoke;self.state("validate")?;
            let deadline=Instant::now()+Duration::from_secs(3);
            loop{
                if wait_signal(&[self.events.decision,self.events.cancel],50)==Some(0){
                    if let Ok(answer)=read_small::<Decision>(&self.path.with_file_name("decision.json"),65536){
                        if answer.token==self.request.token&&answer.validation==self.status.validation{ensure!(answer.permit,"菜单选择已改变");break;}
                    }
                }
                ensure!(!EventSet::signaled(self.events.cancel)&&Instant::now()<deadline&&owner_alive(&self.request),"菜单已取消");
                pump(0);
            }
            self.status.timing.validation_microseconds=self.status.timing.validation_microseconds.saturating_add(micros(started.elapsed()));
            self.state(if before_invoke{"invoking"}else{"tracking"})?;Ok(())
        })();result.map_err(|error|windows::core::Error::new(windows::core::HRESULT(0x80004004u32 as i32),error.to_string()))
    }
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
        let observation=if self.request.options.modern&&unsafe{zs_menu_is_open(hwnd)}!=0{Some("native-presenter-open")}
            else if classic_menu_visible(){Some("visible-classic-menu")}else{None};
        if let Some(observation)=observation{
            self.menu_open.set(true);
            #[cfg(test)]if self.request.probe.as_deref()==Some("menu-dismiss"){unsafe{SetTimer(Some(hwnd),2,80,None);}}
            let value=MenuOpened{token:self.request.token.clone(),pid:std::process::id(),at:monotonic_us(),worker_started:self.worker_started,observation:observation.into(),tracking_started:self.tracking_started.get()};
            if write(&self.path.with_file_name("opened.json"),&value).is_ok(){EventSet::signal(self.status_event);}
            unsafe{SetTimer(Some(hwnd),1,100,None);}
        }
    }
}
unsafe extern "system" fn procedure(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    if msg==WM_NCCREATE{let create=unsafe{&*(lp.0 as*const CREATESTRUCTW)};unsafe{SetWindowLongPtrW(hwnd,GWLP_USERDATA,create.lpCreateParams as isize);}return LRESULT(1);}
    let state=unsafe{GetWindowLongPtrW(hwnd,GWLP_USERDATA)}as*const Watch;
    if !state.is_null(){let state=unsafe{&*state};
        if matches!(msg,WM_ENTERMENULOOP|WM_INITMENUPOPUP){unsafe{SetTimer(Some(hwnd),1,15,None);}
            #[cfg(test)]if state.request.probe.as_deref()==Some("menu-dismiss"){unsafe{SetTimer(Some(hwnd),2,80,None);}}
        }
        #[cfg(test)]if msg==WM_TIMER&&wp.0==2{let _=unsafe{KillTimer(Some(hwnd),2)};let _=zsdesk_windows::shell::menu::dismiss_active_menu(hwnd);let _=unsafe{EndMenu()};return LRESULT(0);}
        if msg==WM_TIMER{
            if !owner_alive(&state.request)||EventSet::signaled(state.cancel_event){let _=zsdesk_windows::shell::menu::dismiss_active_menu(hwnd);let _=unsafe{EndMenu()};}
            state.observe_open(hwnd);
            #[cfg(test)]if matches!(state.request.probe.as_deref(),Some("modern-owner-copy"|"modern-owner-new-folder"))&&state.menu_open.get(){
                if let Some(command)=state.command.take(){let message=unsafe{RegisterWindowMessageW(w!("FILE_EXPLORER_CONTEXTMENU_INVOKEMENUITEM"))};
                    if message!=0{let _=unsafe{PostMessageW(Some(hwnd),message,WPARAM(command as usize),LPARAM(0))};}}
            }
        }
    }
    if let Some(result)=forward_menu_message(msg,wp,lp){return result;}
    unsafe{DefWindowProcW(hwnd,msg,wp,lp)}
}
fn run_request(path:&Path,request:Request,events:&EventSet,com_us:u64,fault_report_available:bool)->Result<bool>{
    let setup=Instant::now();let worker_started=monotonic_us();
    let mut state=Box::new(Worker{status:Status{token:request.token.clone(),pid:std::process::id(),window:0,phase:"starting".into(),validation:0,before_invoke:false,outcome:None,error:None,bind_microseconds:0,populate_microseconds:0,
        reusable:false,layout:None,created:Vec::new(),timing:MenuTimings{worker_thread:unsafe{windows::Win32::System::Threading::GetCurrentThreadId()},fault_report_available,com_microseconds:com_us,..Default::default()},native_menu:serde_json::Value::Null,
        #[cfg(test)]probe_menu_open:false,#[cfg(test)]probe_command_messages:0},request,path:path.into(),events});
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
    let watch=Box::new(Watch{request:state.request.clone(),path:path.into(),status_event:events.status,cancel_event:events.cancel,
        worker_started,menu_open:std::cell::Cell::new(false),tracking_started:std::cell::Cell::new(0),#[cfg(test)]command:std::cell::Cell::new(None)});
    let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW,class,w!("ZSdesk"),WS_POPUP,0,0,0,0,None,None,Some(instance),Some((&*watch as*const Watch).cast()))}.context("create menu owner window")?;
    struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let owned_window=Window(window);
    state.status.window=window.0 as isize;unsafe{SetTimer(Some(window),1,15,None);}
    state.status.timing.worker_setup_microseconds=micros(setup.elapsed());
    let result=(||->Result<MenuOutcome>{
        let request=state.request.clone();zsdesk_windows::shell::menu::set_context_menu_options(request.options.clone());
        #[cfg(test)]if request.probe.as_deref()==Some("dismiss"){state.allow(false)?;return Ok(MenuOutcome::Dismissed);}
        if request.mode==Mode::Properties{
            state.allow(true)?;
            let pidls=if request.pidls.is_empty(){vec![zsdesk_windows::shell::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(request.folder.clone().context("目录缺失")?)).context("目录标识无效")?]}else{request.pidls.clone()};
            let data=zsdesk_windows::desktop::data_object(window,&pidls)?;
            unsafe{windows::Win32::UI::Shell::SHMultiFileProperties(&data,0)}?;
            return Ok(MenuOutcome::ShellInvoked);
        }
        let started=Instant::now();
        let background=if matches!(request.mode,Mode::Background|Mode::New){Some(zsdesk_windows::shell::pidl::item_from_pidl(&request.pidls[0])?)}else{None};
        let background_view=background.as_ref().map(|item|background_view::BackgroundView::create(item,&request,events)).transpose()?;
        if let (Some(host),Some(layout))=(&background_view,&request.layout){zsdesk_windows::shell::restore_view_layout(&host.view,layout);}
        let initial_layout=background_view.as_ref().and_then(|host|zsdesk_windows::shell::capture_view_layout(&host.view));
        let menu=if let Some(host)=&background_view{
            if request.mode==Mode::New{zsdesk_windows::shell::new_menu::for_view(&host.view)?}else{ShellMenu::for_view(&host.view,false)?}
        }else if request.pidls.is_empty(){let folder=zsdesk_windows::desktop::folder_for_path(request.folder.as_deref().context("目录缺失")?)?;ShellMenu::for_folder_background(window,&folder)?}else{ShellMenu::for_items(window,&request.pidls)?};
        let creation=background_view.as_ref().filter(|_|request.folder.is_some()).map(|host|zsdesk_windows::shell::new_menu::CreationCapture::attach(&menu,&host.view)).transpose()?;
        state.status.bind_microseconds=started.elapsed().as_micros()as u64;let started=Instant::now();
        if request.mode==Mode::Open{
            menu.populate_for_invoke(request.extended)?;state.status.populate_microseconds=started.elapsed().as_micros()as u64;
            state.allow(true)?;menu.invoke_verb(window,"open",false)?;return Ok(MenuOutcome::ShellInvoked);
        }
        menu.populate(request.rename,request.extended)?;state.status.populate_microseconds=started.elapsed().as_micros()as u64;
        state.allow(false)?;
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
        let foreground_started=Instant::now();
        unsafe{let _=ShowWindow(window,SW_SHOWNOACTIVATE);let _=SetForegroundWindow(window);}
        state.status.timing.foreground_microseconds=micros(foreground_started.elapsed());
        let point=POINT{x:request.point[0],y:request.point[1]};
        #[cfg(test)]let probe_outcome=if matches!(request.probe.as_deref(),Some("new-folder"|"new-text")){
            let command=isolation_tests::new_item_command(&menu,request.probe.as_deref()==Some("new-text"))?;
            state.allow(true)?;menu.invoke(window,command,None,false,false)?;Some(MenuOutcome::ShellInvoked)
        }else if request.probe.as_deref()==Some("background-details"){
            ensure!(background_view.is_some(),"background view required");let command=isolation_tests::background_details_command(&menu)?;
            state.allow(true)?;menu.invoke(window,command,None,false,false)?;Some(MenuOutcome::ShellInvoked)
        }else{None};
        #[cfg(not(test))]let probe_outcome:Option<MenuOutcome>=None;
        watch.tracking_started.set(monotonic_us());
        let outcome=if let Some(outcome)=probe_outcome{outcome}
            else if request.mode==Mode::New{menu.track_new(window,point,&mut||state.allow(true))?}
            else if let (Some(item),Some(host))=(&background,&background_view){menu.track_background_view(window,point,item,&host.browser,&mut||state.allow(true))?}
            else if request.pidls.is_empty(){menu.track_folder_background(window,point,request.folder.as_deref().unwrap(),0,&mut||state.allow(true))?}
            else{menu.track_selected_before(window,point,&request.pidls,0,MenuInterceptions{rename:request.rename,open:request.open},&mut||state.allow(true))?};
        if matches!(outcome,MenuOutcome::ShellInvoked|MenuOutcome::NativeHandled){
            pump(150);
            // ShellNew can open a modeless shortcut/template wizard. Its site
            // must remain alive until the wizard finishes its create callback.
            if creation.is_some(){while process_visible_windows(std::process::id(),window){pump(50);}}
            if let Some(creation)=&creation{state.status.created=creation.take();}
            let layout=background_view.as_ref().and_then(|host|zsdesk_windows::shell::capture_view_layout(&host.view));
            if layout!=initial_layout{state.status.layout=layout;}
        }
        Ok(outcome)
    })();

    match result{
        Ok(outcome)=>state.status.outcome=Some(outcome),
        Err(error) if error.downcast_ref::<windows::core::Error>().is_some_and(|error|matches!(error.code().0 as u32,0x80004004|0x800704c7))=>state.status.outcome=Some(MenuOutcome::Dismissed),
        Err(error)=>state.status.error=Some(format!("{error:#}")),
    }
    #[cfg(test)]{state.status.probe_menu_open=watch.menu_open.get();state.status.probe_command_messages=unsafe{zs_menu_command_messages()};}
    let persistence_deadline=Instant::now()+Duration::from_secs(1);
    let persisted=loop{
        if !owns_clipboard(){break true;}
        match zsdesk_windows::shell::file_transfer::persist_owned_file_clipboard(window){
            Ok(persisted)=>break persisted,
            Err(error) if matches!(error.code().0 as u32,0x800401d0|0x80070005|0x800700aa)&&Instant::now()<persistence_deadline=>pump(25),
            Err(_)=>break false,
        }
    };
    if state.status.before_invoke{
        // Property sheets and similar command UI appear asynchronously; let
        // them become visible before deciding whether this process is idle.
        pump(150);
    }
    let reusable=state.status.error.is_none()&&state.request.ordinal<MAX_SESSION_REQUESTS
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

fn run_worker(path:&Path)->Result<()>{
    ensure!(path.is_absolute()&&path.file_name().is_some_and(|name|name=="request.json"),"菜单请求路径无效");
    ensure!(std::env::var_os("ZSDESK_MENU_WORKSPACE").map(PathBuf::from).as_deref()==path.parent(),"菜单工作目录不匹配");
    let session=std::env::var("ZSDESK_MENU_SESSION").context("菜单会话缺失")?;
    let mut request:Request=read_small(path,MAX_REQUEST)?;validate(&request)?;
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
    // An idle process may still host UI or clipboard data of a command it ran
    // for an earlier request (it is reusable while none is visible yet). Never
    // end those with the process.
    let linger=||{
        while process_visible_windows(std::process::id(),HWND::default()){pump(50);}
        while owns_clipboard(){pump(50);}
    };
    loop{
        let ordinal=request.ordinal;let token=request.token.clone();
        if !run_request(path,request, &events,com_us,fault_report_available)?{return Ok(());}com_us=0;
        let deadline=Instant::now()+IDLE_TIMEOUT;
        loop{
            if EventSet::signaled(events.cancel)||Instant::now()>=deadline||created(parent.0)!=Some(parent.1){linger();return Ok(());}
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
    #[test]fn menu_protocol_rejects_unbounded_or_path_like_sessions(){
        let mut request=Request{token:"request-1".into(),session:"session-1".into(),ordinal:1,parent:1,parent_created:1,owner:1,
            point:[0,0],pidls:vec![vec![0,0]],folder:None,options:Default::default(),rename:false,open:false,mode:Mode::More,extended:false,layout:None,probe:None};
        assert!(validate(&request).is_ok());
        for value in ["",r"..\escape","bad/session","bad\0session"]{request.session=value.into();assert!(validate(&request).is_err());}
        request.session="session-1".into();for ordinal in [0,MAX_SESSION_REQUESTS+1]{request.ordinal=ordinal;assert!(validate(&request).is_err());}
    }
    #[test]fn menu_events_wake_once_but_cancellation_stays_latched()->Result<()>{
        let events=EventSet::create(&format!("test-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed)))?;
        EventSet::signal(events.status);assert_eq!(wait_signal(&[events.status],0),Some(0));assert!(!EventSet::signaled(events.status));
        EventSet::signal(events.cancel);assert!(EventSet::signaled(events.cancel));assert!(EventSet::signaled(events.cancel));
        events.reset();assert!(!EventSet::signaled(events.cancel));Ok(())
    }
    #[test]fn menu_diagnostics_omit_user_paths_but_keep_error_codes(){
        let value=redacted_error("0xC0000005 failed C:\\Users\\Example\\private file.txt\nmodule WPS.dll\nnetwork \\\\server\\private");
        assert!(value.contains("0xC0000005")&&value.contains("WPS.dll"));
        assert!(!value.contains("Example")&&!value.contains("server")&&!value.contains("private"));
    }
    #[test]#[ignore="Internal subprocess entry for menu isolation regression"]
    fn native_extension_worker_entry()->Result<()>{
        #[link(name="user32")]unsafe extern "system"{fn OpenDesktopW(name:*const u16,flags:u32,inherit:i32,access:u32)->*mut core::ffi::c_void;fn SetThreadDesktop(desktop:*mut core::ffi::c_void)->i32;}
        if let Ok(name)=std::env::var("ZSDESK_MENU_TEST_DESKTOP"){
            let desktop=unsafe{OpenDesktopW(wide(&name).as_ptr(),0,0,0x000f00ff)};ensure!(!desktop.is_null()&&unsafe{SetThreadDesktop(desktop)}!=0,"private worker desktop unavailable");
        }
        let root=PathBuf::from(std::env::var_os("ZSDESK_MENU_WORKSPACE").context("workspace")?);run_worker(&root.join("request.json"))
    }
}
#[cfg(test)]#[path="context_menu_process_tests.rs"]pub(crate) mod isolation_tests;
