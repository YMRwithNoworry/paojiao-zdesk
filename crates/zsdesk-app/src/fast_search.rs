//! Explicit, mutually exclusive search backends with bounded asynchronous results.
mod ntfs;
mod metadata;
mod native_scan;
mod snapshot;
mod query;
mod native;
#[cfg(test)]#[path="fast_search/runtime_regression.rs"]mod runtime_regression;
pub(crate) mod everything;
pub use zsdesk_core::model::explorer::SearchBackendPreference as BackendPreference;
use std::{collections::HashMap,ffi::{OsStr,OsString},fs,os::windows::ffi::{OsStrExt,OsStringExt},path::{Path,PathBuf},
    sync::{Arc,Weak,Mutex,Condvar,OnceLock,RwLock,atomic::{AtomicBool,AtomicU64,Ordering},mpsc::{self,Receiver,SyncSender,TrySendError}},
    thread::{self,JoinHandle},time::{Duration,Instant}};
use anyhow::{Result,Context,bail,ensure};
use windows::{core::PCWSTR,Win32::{Foundation::{HWND,LPARAM,WPARAM},Storage::FileSystem::{GetDriveTypeW,GetLogicalDriveStringsW},UI::WindowsAndMessaging::PostMessageW}};

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct SearchOptions{pub backend:BackendPreference,pub include_development:bool,pub index_metadata:bool,pub pinyin_initials:bool,pub idle_seconds:u32}
impl Default for SearchOptions{fn default()->Self{Self{backend:BackendPreference::NativeNtfs,include_development:false,index_metadata:false,pinyin_initials:false,idle_seconds:300}}}
impl SearchOptions{
    pub fn from_ui(ui:&zsdesk_core::model::explorer::ExplorerUi)->Self{Self{backend:ui.search_backend,include_development:ui.search_include_development,index_metadata:ui.search_index_metadata,pinyin_initials:ui.search_pinyin_initials,idle_seconds:ui.search_idle_seconds.clamp(30,3600)}}
    fn effective(self,profile:&Path)->Result<Self>{crate::storage::load(profile).map(|session|Self::from_ui(&session.ui)).context("无法读取搜索后端设置，请重新保存搜索设置")}
}
#[derive(Clone,Copy,Debug,Default,serde::Serialize,PartialEq,Eq)]#[serde(rename_all="snake_case")]
pub enum MaintenanceRole{#[default]Pending,Writer,Reader}
#[derive(Clone,Copy,Debug,Default,serde::Serialize,serde::Deserialize,PartialEq,Eq)]#[serde(rename_all="snake_case")]
pub enum BackendKind{NativeNtfs,#[default]DirectoryScan,Mixed,Everything}
#[derive(Clone,Copy,Debug,Default,serde::Serialize,serde::Deserialize,PartialEq,Eq)]#[serde(rename_all="snake_case")]
pub enum IndexPhase{#[default]Starting,Snapshot,CatchingUp,Scanning,Attributes,Ready,Paused}
#[derive(Clone,Debug,serde::Serialize,serde::Deserialize,PartialEq,Eq)]
pub struct VolumeStatus{pub root:PathBuf,pub backend:BackendKind,pub phase:IndexPhase,pub indexed:u64,pub checkpoint:Option<i64>,pub warning:Option<String>}
#[derive(Clone,Debug)]
pub struct SearchEvent{
    pub generation:u64,pub paths:Vec<PathBuf>,pub scanned:u64,pub indexed:u64,pub denied:u64,pub skipped_reparse:u64,
    pub scanning:bool,pub finished:bool,pub truncated:bool,pub error:Option<String>,pub reset:bool,
    pub backend:BackendKind,pub phase:IndexPhase,pub volumes:Vec<VolumeStatus>,pub read_only:bool,
}
#[derive(Clone)]struct Request{generation:u64,query:String,preferred:Option<PathBuf>,invalid:Option<String>,offset:u32,limit:u32,sort:everything::SearchSort}
#[derive(Clone,Copy,Default,PartialEq)]enum ScanAction{#[default]None,Refresh,Rebuild}
#[derive(Default)]struct Control{request:Option<Request>,action:ScanAction,release_idle:bool,settings_changed:bool}
struct Shared{
    control:Mutex<Control>,wake:Condvar,stop:AtomicBool,engine_stop:AtomicBool,generation:AtomicU64,notified:AtomicBool,
    hwnd:isize,message:u32,notifier:Option<Arc<dyn Fn()+Send+Sync>>,maintenance_role:AtomicU64,
}
impl Shared{
    fn current(&self,generation:u64)->bool{!self.stop.load(Ordering::Acquire)&&self.generation.load(Ordering::Acquire)==generation}
    fn cancelled(&self)->bool{self.stop.load(Ordering::Acquire)||self.engine_stop.load(Ordering::Acquire)}
    fn notify(&self){
        if self.stop.load(Ordering::Acquire)||(self.notifier.is_none()&&(self.hwnd==0||self.message==0))||self.notified.swap(true,Ordering::AcqRel){return;}
        if let Some(notify)=&self.notifier{notify();return;}
        if unsafe{PostMessageW(Some(HWND(self.hwnd as*mut _)),self.message,WPARAM(0),LPARAM(0))}.is_err(){self.notified.store(false,Ordering::Release);}
    }
}
type Registry=HashMap<PathBuf,Vec<Weak<Shared>>>;
static CLIENTS:OnceLock<Mutex<Registry>>=OnceLock::new();
static EVERYTHING_ACTIVITY:RwLock<()>=RwLock::new(());
pub(super) fn everything_activity()->std::sync::RwLockReadGuard<'static,()>{EVERYTHING_ACTIVITY.read().unwrap_or_else(|e|e.into_inner())}
pub fn require_everything(profile:&Path)->Result<()>{
    ensure!(crate::storage::load(profile)?.ui.search_backend==BackendPreference::Everything,"当前选择自带搜索；Everything 不会自动启动，请在搜索设置中切换后端");Ok(())
}
pub fn settings_changed(profile:&Path){
    if let Some(clients)=CLIENTS.get(){let mut clients=clients.lock().unwrap_or_else(|e|e.into_inner());
        if let Some(clients)=clients.get_mut(&profile.to_path_buf()){clients.retain(|client|{let Some(client)=client.upgrade()else{return false;};
            client.engine_stop.store(true,Ordering::Release);client.generation.fetch_add(1,Ordering::AcqRel);
            let mut control=client.control.lock().unwrap_or_else(|e|e.into_inner());control.request=None;control.settings_changed=true;control.release_idle=true;drop(control);client.wake.notify_all();true});}
    }
    let profile=profile.to_path_buf();
    let _=thread::Builder::new().name("zsdesk-search-release".into()).spawn(move||{
        let _activity=EVERYTHING_ACTIVITY.write().unwrap_or_else(|e|e.into_inner());
        if require_everything(&profile).is_ok(){return;}
        let peers=CLIENTS.get().map(|clients|clients.lock().unwrap_or_else(|e|e.into_inner()).iter().filter(|(_,clients)|clients.iter().any(|c|c.strong_count()>0)).map(|(path,_)|path.clone()).collect::<Vec<_>>()).unwrap_or_default();
        if peers.iter().any(|path|require_everything(path).is_ok()){return;}
        let _=everything::shutdown_managed_if_unused();
    });
}

pub struct FastSearch{shared:Arc<Shared>,events:Receiver<SearchEvent>,worker:Option<JoinHandle<()>>}
impl FastSearch{
    pub fn new(path:PathBuf,hwnd:HWND,message:u32)->Result<Self>{Self::new_with_options(path,hwnd,message,SearchOptions::default())}
    pub fn new_with_options(path:PathBuf,hwnd:HWND,message:u32,options:SearchOptions)->Result<Self>{Self::create(path,hwnd,message,options,None,None,None)}
    pub fn new_with_notifier(path:PathBuf,options:SearchOptions,notifier:Arc<dyn Fn()+Send+Sync>)->Result<Self>{Self::create(path,HWND::default(),0,options,None,Some(notifier),None)}
    pub fn for_profile(profile:&Path,hwnd:HWND,message:u32,options:SearchOptions)->Result<Self>{
        use sha2::{Digest,Sha256};let profile=std::path::absolute(profile)?;
        #[cfg(test)]let path=profile.with_file_name("search-index.zsi");
        #[cfg(not(test))]let path={let identity=fs::canonicalize(&profile).map(|p|path_key_native(&p)).unwrap_or_else(|_|profile.clone());
            let hash=format!("{:x}",Sha256::digest(identity.to_string_lossy().as_bytes()));
            dirs::cache_dir().context("无法定位本地搜索缓存目录")?.join(crate::edition::profile_name()).join("search").join(hash).join("index.zsi")};
        Self::create(path,hwnd,message,options,None,None,Some(profile))
    }
    #[cfg(test)]pub(crate) fn new_with_roots_for_test(path:PathBuf,roots:Vec<PathBuf>)->Result<Self>{Self::create(path,HWND::default(),0,SearchOptions{backend:BackendPreference::DirectoryScan,..Default::default()},Some(roots),None,None)}
    fn create(path:PathBuf,hwnd:HWND,message:u32,options:SearchOptions,roots:Option<Vec<PathBuf>>,notifier:Option<Arc<dyn Fn()+Send+Sync>>,profile:Option<PathBuf>)->Result<Self>{
        let path=std::path::absolute(path)?;let profile=profile.unwrap_or_else(||path.with_file_name("explorer-session.json"));
        let shared=Arc::new(Shared{control:Mutex::new(Control::default()),wake:Condvar::new(),stop:AtomicBool::new(false),engine_stop:AtomicBool::new(false),
            generation:AtomicU64::new(0),notified:AtomicBool::new(false),hwnd:hwnd.0 as isize,message,notifier,maintenance_role:AtomicU64::new(0)});
        CLIENTS.get_or_init(Default::default).lock().unwrap_or_else(|e|e.into_inner()).entry(profile.clone()).or_default().push(Arc::downgrade(&shared));
        let (sender,events)=mpsc::sync_channel(4);let state=shared.clone();
        let worker=thread::Builder::new().name("zsdesk-search".into()).spawn(move||worker_loop(path,profile,options,roots,state,sender))?;
        Ok(Self{shared,events,worker:Some(worker)})
    }
    pub fn start(&self,query:&str,preferred:Option<PathBuf>)->u64{self.start_page(query,preferred,0,2000,everything::SearchSort::NameAsc)}
    pub fn start_page(&self,query:&str,preferred:Option<PathBuf>,offset:u32,limit:u32,sort:everything::SearchSort)->u64{
        let generation=self.shared.generation.fetch_add(1,Ordering::AcqRel)+1;self.discard_events();
        let text:String=query.trim().chars().take(513).collect();let invalid=if text.chars().count()>512||text.contains('\0'){Some("搜索内容过长或包含无效字符".into())}else{None};
        let mut control=self.shared.control.lock().unwrap_or_else(|e|e.into_inner());control.release_idle=text.is_empty();
        control.request=(!text.is_empty()).then(||Request{generation,query:text,preferred,invalid,offset,limit:limit.clamp(1,2000),sort});
        self.shared.engine_stop.store(control.request.is_none(),Ordering::Release);drop(control);self.shared.wake.notify_all();generation
    }
    pub fn cancel(&self){self.shared.generation.fetch_add(1,Ordering::AcqRel);self.shared.engine_stop.store(true,Ordering::Release);
        let mut control=self.shared.control.lock().unwrap_or_else(|e|e.into_inner());control.request=None;control.action=ScanAction::None;control.release_idle=true;drop(control);self.discard_events();self.shared.wake.notify_all();}
    pub fn refresh(&self){self.schedule(ScanAction::Refresh);}
    pub fn rebuild(&self)->u64{self.schedule(ScanAction::Rebuild);self.shared.generation.load(Ordering::Acquire)}
    fn schedule(&self,action:ScanAction){self.shared.generation.fetch_add(1,Ordering::AcqRel);self.shared.engine_stop.store(false,Ordering::Release);
        let mut control=self.shared.control.lock().unwrap_or_else(|e|e.into_inner());control.action=action;control.release_idle=false;drop(control);self.discard_events();self.shared.wake.notify_all();}
    pub fn maintenance_role(&self)->MaintenanceRole{match self.shared.maintenance_role.load(Ordering::Acquire){1=>MaintenanceRole::Writer,2=>MaintenanceRole::Reader,_=>MaintenanceRole::Pending}}
    pub fn poll(&self)->Vec<SearchEvent>{self.shared.notified.store(false,Ordering::Release);let generation=self.shared.generation.load(Ordering::Acquire);let mut events=Vec::new();let mut drained=0;
        for _ in 0..4{let Ok(event)=self.events.try_recv()else{break;};drained+=1;if event.generation==generation{events.push(event);}}
        if drained==4{self.shared.notify();}events}
    fn discard_events(&self){self.shared.notified.store(false,Ordering::Release);for _ in 0..4{if self.events.try_recv().is_err(){break;}}}
}
impl Drop for FastSearch{fn drop(&mut self){self.shared.stop.store(true,Ordering::Release);self.shared.engine_stop.store(true,Ordering::Release);self.shared.wake.notify_all();if let Some(worker)=self.worker.take(){if worker.is_finished(){let _=worker.join();}}}}

fn emit(shared:&Shared,sender:&SyncSender<SearchEvent>,mut event:SearchEvent){
    loop{if !shared.current(event.generation){return;}match sender.try_send(event){Ok(())=>{shared.notify();return;},Err(TrySendError::Full(value))=>{event=value;thread::sleep(Duration::from_millis(5));},Err(TrySendError::Disconnected(_))=>return,}}
}
fn failure(generation:u64,backend:BackendKind,error:String)->SearchEvent{SearchEvent{generation,paths:Vec::new(),scanned:0,indexed:0,denied:0,skipped_reparse:0,scanning:false,finished:true,truncated:false,error:Some(error),reset:false,backend,phase:IndexPhase::Paused,volumes:Vec::new(),read_only:false}}
fn worker_loop(path:PathBuf,profile:PathBuf,initial:SearchOptions,roots:Option<Vec<PathBuf>>,shared:Arc<Shared>,sender:SyncSender<SearchEvent>){
    let mut core:Option<native::Core>=None;
    let mut background=false;
    loop{
        let control={let mut guard=shared.control.lock().unwrap_or_else(|e|e.into_inner());
            if guard.request.is_none()&&guard.action==ScanAction::None&&!guard.settings_changed&&!shared.stop.load(Ordering::Acquire){
                let result=shared.wake.wait_timeout(guard,Duration::from_secs(1)).unwrap_or_else(|e|e.into_inner());guard=result.0;
            }
            if shared.stop.load(Ordering::Acquire){break;}std::mem::take(&mut *guard)
        };
        let options=if roots.is_some(){initial}else{match initial.effective(&profile){Ok(options)=>options,Err(error)=>{
            core=None;if let Some(request)=control.request{emit(&shared,&sender,failure(request.generation,if initial.backend.is_builtin(){BackendKind::DirectoryScan}else{BackendKind::Everything},format!("{error:#}")));}continue;
        }}};
        if options.backend.is_builtin()!=background{
            use windows::Win32::System::Threading::{GetCurrentThread,SetThreadPriority,THREAD_MODE_BACKGROUND_BEGIN,THREAD_MODE_BACKGROUND_END};
            let _=unsafe{SetThreadPriority(GetCurrentThread(),if options.backend.is_builtin(){THREAD_MODE_BACKGROUND_BEGIN}else{THREAD_MODE_BACKGROUND_END})};background=options.backend.is_builtin();
        }
        if !options.backend.is_builtin(){core=None;}else if control.request.is_none()&&control.action==ScanAction::None{if let Some(core)=&core{native::evict_if_idle(core,options.idle_seconds);}}
        if control.settings_changed{core=None;}
        let Some(request)=control.request else{
            if control.action!=ScanAction::None&&options.backend.is_builtin(){
                let generation=shared.generation.load(Ordering::Acquire);shared.maintenance_role.store(1,Ordering::Release);
                let core=core.get_or_insert_with(||native::shared(&path));
                if control.action==ScanAction::Rebuild{native::invalidate(core);}else{native::refresh(core);}
                let result=native::prepare(core,&options,roots.as_deref(),&shared,&sender);shared.maintenance_role.store(0,Ordering::Release);
                let mut event=failure(generation,BackendKind::DirectoryScan,String::new());event.error=result.err().map(|e|format!("{e:#}"));
                if event.error.is_none(){event.phase=IndexPhase::Ready;}emit(&shared,&sender,event);
            }continue;
        };
        if !shared.current(request.generation){continue;}
        if let Some(error)=&request.invalid{emit(&shared,&sender,failure(request.generation,if options.backend.is_builtin(){BackendKind::DirectoryScan}else{BackendKind::Everything},error.clone()));continue;}
        if options.backend.is_builtin(){
            shared.maintenance_role.store(1,Ordering::Release);let core=core.get_or_insert_with(||native::shared(&path));
            if control.action==ScanAction::Rebuild{native::invalidate(core);}
            if let Err(error)=native::execute(core,&options,roots.as_deref(),&shared,&sender,&request){if shared.current(request.generation){emit(&shared,&sender,failure(request.generation,BackendKind::DirectoryScan,format!("{error:#}")));}}
        }else{
            shared.maintenance_role.store(2,Ordering::Release);
            everything::execute(&shared,&sender,&request,&profile,crate::storage::load(&profile).ok().and_then(|s|s.ui.everything_executable));
        }
        shared.maintenance_role.store(0,Ordering::Release);
    }
}
pub(super) fn path_key_native(path:&Path)->PathBuf{
    let units:Vec<_>=path.as_os_str().encode_wide().collect();let prefix:Vec<_>="\\\\?\\".encode_utf16().collect();
    if units.starts_with(&prefix){let rest=OsString::from_wide(&units[4..]);let text=rest.to_string_lossy();if let Some(unc)=text.strip_prefix("UNC\\"){return PathBuf::from(format!("\\\\{unc}"));}return PathBuf::from(rest);}path.into()
}
fn fixed_roots()->Vec<PathBuf>{let mut buffer=[0u16;512];let length=unsafe{GetLogicalDriveStringsW(Some(&mut buffer))}as usize;
    if length==0||length>=buffer.len(){return Vec::new();}buffer[..length].split(|v|*v==0).filter(|s|!s.is_empty()).filter_map(|units|{let text:Vec<_>=units.iter().copied().chain(Some(0)).collect();
        (unsafe{GetDriveTypeW(PCWSTR(text.as_ptr()))}==3).then(||PathBuf::from(OsString::from_wide(units)))}).collect()}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn selected_backend_survives_normalization(){
        let mut session=zsdesk_core::model::explorer::ExplorerSession::default();
        for backend in [BackendPreference::NativeNtfs,BackendPreference::Everything]{session.ui.search_backend=backend;session.normalize();assert_eq!(session.ui.search_backend,backend);}
    }
}
