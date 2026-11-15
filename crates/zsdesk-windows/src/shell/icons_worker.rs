//! Bounded Shell extraction in owned, reusable helper processes.
//!
//! Only fixed protocol files live in a private per-worker directory. No native
//! image decoder runs on the desktop STA or on a parent loader thread.
use super::{load,IconRequest,MAX_READY_BYTES};
use crate::gfx::{BitmapAlpha,Gfx};
use std::{cell::RefCell,ffi::OsString,fs,io::{self,Read,Write},marker::PhantomData,
    os::windows::{fs::MetadataExt,io::AsRawHandle,process::CommandExt},path::{Path,PathBuf,Prefix},
    process::{Child,Command,Stdio},rc::Rc,sync::{Mutex,OnceLock,atomic::{AtomicU64,AtomicUsize,Ordering}},
    thread,time::{Duration,Instant,SystemTime,UNIX_EPOCH}};
use serde::{Deserialize,Serialize};
use windows::{core::{Interface,PCWSTR},Win32::{Foundation::{CloseHandle,HANDLE,WAIT_OBJECT_0},
    Graphics::Gdi::{CreateDIBSection,DeleteObject,HBITMAP,BITMAPINFO,BITMAPINFOHEADER,DIB_RGB_COLORS,BI_RGB},
    Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING},
    System::{Com::{CoInitializeEx,CoUninitialize,COINIT_APARTMENTTHREADED,COINIT_DISABLE_OLE1DDE},JobObjects::*,Threading::*}}};

const FLAG:&str="--icon-worker";
#[link(name="kernel32")]unsafe extern "system"{fn SetErrorMode(mode:u32)->u32;}
const TEST_REQUEST:&str="ZSDESK_ICON_TEST_REQUEST";
const MAX_REQUEST:u64=512*1024;
const HEADER:usize=32;
const MAGIC:&[u8;8]=b"ZSDICON1";
pub(super) const IDLE:Duration=Duration::from_secs(20);
const START_TIMEOUT:Duration=Duration::from_secs(3);
const ICON_TIMEOUT:Duration=Duration::from_millis(750);
const THUMBNAIL_TIMEOUT:Duration=Duration::from_secs(3);
static NEXT:AtomicU64=AtomicU64::new(1);
static ACTIVE:AtomicUsize=AtomicUsize::new(0);
static TIMEOUTS:AtomicU64=AtomicU64::new(0);
static CANCELLED:AtomicU64=AtomicU64::new(0);
static CLEANUP_ERRORS:AtomicU64=AtomicU64::new(0);
const MAX_WORKERS:usize=2;
static RETIRED:OnceLock<Mutex<Vec<Retired>>>=OnceLock::new();
thread_local!{static TEST_ENTRIES:RefCell<Vec<(u64,Vec<OsString>)>>=const{RefCell::new(Vec::new())};}

/// The same libtest-entry pattern used by packaged command workers. Captured
/// by IconLoader::spawn before its background threads are created.
pub struct TestEntryGuard{id:u64,_thread:PhantomData<Rc<()>>}
impl Drop for TestEntryGuard{fn drop(&mut self){TEST_ENTRIES.with(|entries|entries.borrow_mut().retain(|(id,_)|*id!=self.id));}}
pub fn test_entry(prefix:&[OsString])->io::Result<TestEntryGuard>{
    use std::os::windows::ffi::OsStrExt;
    if prefix.is_empty()||prefix.len()>8||prefix.iter().any(|part|part.encode_wide().any(|unit|unit==0)){return Err(invalid("invalid icon test entry"));}
    let id=NEXT.fetch_add(1,Ordering::Relaxed);TEST_ENTRIES.with(|entries|entries.borrow_mut().push((id,prefix.to_vec())));
    Ok(TestEntryGuard{id,_thread:PhantomData})
}
pub fn run_test_entry()->io::Result<()>{
    let path=std::env::var_os(TEST_REQUEST).ok_or_else(||invalid("icon test request missing"))?;
    run(Path::new(&path))
}
pub fn pending_workers()->usize{ACTIVE.load(Ordering::Acquire)}
pub fn diagnostics()->serde_json::Value{serde_json::json!({"active":pending_workers(),"limit":MAX_WORKERS,"timeouts":TIMEOUTS.load(Ordering::Relaxed),"cancelled":CANCELLED.load(Ordering::Relaxed),"cleanup_errors":CLEANUP_ERRORS.load(Ordering::Relaxed),"icon_timeout_ms":ICON_TIMEOUT.as_millis(),"thumbnail_timeout_ms":THUMBNAIL_TIMEOUT.as_millis()})}

#[derive(Clone)]pub(super) struct Config{executable:PathBuf,root:PathBuf,prefix:Option<Vec<OsString>>}
impl Config{
    pub(super) fn current()->io::Result<Self>{
        let root=std::env::var_os("ZSDESK_ICON_WORKER_ROOT").map(PathBuf::from).unwrap_or_else(||
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("ZSDesk").join("icons"));
        local_path(&root)?;real_ancestors(&root)?;
        let prefix=TEST_ENTRIES.with(|entries|entries.borrow().last().map(|row|row.1.clone()));
        Ok(Self{executable:std::env::current_exe()?,root,prefix})
    }
}
fn invalid(message:&str)->io::Error{io::Error::new(io::ErrorKind::InvalidData,message)}
fn windows_error(error:windows::core::Error)->io::Error{io::Error::other(error.to_string())}
fn wide(path:&Path)->Vec<u16>{use std::os::windows::ffi::OsStrExt;path.as_os_str().encode_wide().chain(Some(0)).collect()}
fn local_path(path:&Path)->io::Result<()>{
    if !path.is_absolute()||!matches!(path.components().next(),Some(std::path::Component::Prefix(prefix))if matches!(prefix.kind(),Prefix::Disk(_)|Prefix::VerbatimDisk(_))){return Err(invalid("icon workspace must be an absolute local-disk path"));}Ok(())
}
fn real_ancestors(path:&Path)->io::Result<()>{
    for part in path.ancestors(){match fs::symlink_metadata(part){Ok(metadata)=>if metadata.file_attributes()&0x400!=0{return Err(invalid("icon workspace contains a reparse point"));},Err(error)if error.kind()==io::ErrorKind::NotFound=>{},Err(error)=>return Err(error)}}Ok(())
}
fn bounded_read(path:&Path,limit:u64)->io::Result<Vec<u8>>{
    let metadata=fs::symlink_metadata(path)?;
    if !metadata.is_file()||metadata.file_attributes()&0x400!=0||metadata.len()>limit{return Err(invalid("invalid icon protocol file"));}
    let mut bytes=Vec::with_capacity(metadata.len()as usize);fs::File::open(path)?.take(limit+1).read_to_end(&mut bytes)?;
    if bytes.len()as u64>limit{return Err(invalid("icon protocol file exceeds bound"));}Ok(bytes)
}
fn atomic_write(root:&Path,name:&str,bytes:&[u8])->io::Result<()>{
    let target=root.join(name);let temporary=root.join(format!("{name}.tmp"));
    real_ancestors(root)?;
    for path in [&target,&temporary]{if let Ok(metadata)=fs::symlink_metadata(path){if !metadata.is_file()||metadata.file_attributes()&0x400!=0{return Err(invalid("invalid icon protocol output"));}}}
    {let mut file=fs::OpenOptions::new().write(true).create(true).truncate(true).open(&temporary)?;file.write_all(bytes)?;}
    unsafe{MoveFileExW(PCWSTR(wide(&temporary).as_ptr()),PCWSTR(wide(&target).as_ptr()),MOVEFILE_REPLACE_EXISTING)}.map_err(windows_error)
}
fn cleanup(root:&Path){
    if !root.file_name().is_some_and(|name|name.to_string_lossy().starts_with("icon-worker-"))||real_ancestors(root).is_err(){CLEANUP_ERRORS.fetch_add(1,Ordering::Relaxed);return;}
    for name in ["owner.json","owner.json.tmp","worker.json","worker.json.tmp","request.json","request.json.tmp","response.bin","response.bin.tmp","ready.json","ready.json.tmp"]{
        let path=root.join(name);match fs::symlink_metadata(&path){
            Ok(metadata)if metadata.is_file()&&metadata.file_attributes()&0x400==0=>{if fs::remove_file(path).is_err(){CLEANUP_ERRORS.fetch_add(1,Ordering::Relaxed);}},
            Ok(_)=>{CLEANUP_ERRORS.fetch_add(1,Ordering::Relaxed);},Err(error)if error.kind()==io::ErrorKind::NotFound=>{},Err(_)=>{CLEANUP_ERRORS.fetch_add(1,Ordering::Relaxed);},
        }
    }
    if fs::remove_dir(root).is_err(){CLEANUP_ERRORS.fetch_add(1,Ordering::Relaxed);}
}
#[derive(Serialize,Deserialize)]struct Owner{schema:u32,parent:u32,created:u64,session:String}
#[derive(Serialize,Deserialize)]struct Request{schema:u32,session:String,serial:u64,token:u64,size:u32,thumbnail:bool,pidl:Vec<u8>}
#[derive(Serialize,Deserialize)]struct Ready{schema:u32,session:String,pid:u32}
fn process_created(handle:HANDLE)->io::Result<u64>{
    let(mut creation,mut exit,mut kernel,mut user)=Default::default();
    unsafe{GetProcessTimes(handle,&mut creation,&mut exit,&mut kernel,&mut user)}.map_err(windows_error)?;
    Ok((u64::from(creation.dwHighDateTime)<<32)|u64::from(creation.dwLowDateTime))
}

struct Job(HANDLE);
unsafe impl Send for Job{}
impl Drop for Job{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}
struct Permit;
impl Permit{fn acquire()->io::Result<Self>{
    let mut count=ACTIVE.load(Ordering::Acquire);
    loop{if count>=MAX_WORKERS{return Err(io::Error::new(io::ErrorKind::WouldBlock,"icon worker limit reached"));}
        match ACTIVE.compare_exchange_weak(count,count+1,Ordering::AcqRel,Ordering::Acquire){Ok(_)=>return Ok(Self),Err(actual)=>count=actual}}
}}
impl Drop for Permit{fn drop(&mut self){ACTIVE.fetch_sub(1,Ordering::AcqRel);}}
struct Retired{child:Child,workspace:PathBuf,_permit:Permit}
fn reap_retired(){
    let Some(retired)=RETIRED.get()else{return;};let mut finished=Vec::new();
    {let mut retired=retired.lock().unwrap_or_else(|error|error.into_inner());let mut index=0;
        while index<retired.len(){if retired[index].child.try_wait().ok().flatten().is_some(){finished.push(retired.swap_remove(index));}else{index+=1;}}
    }
    for retired in finished{cleanup(&retired.workspace);}
}
pub(super) struct Worker{child:Option<Child>,job:Option<Job>,permit:Option<Permit>,workspace:PathBuf,owner:Owner,serial:u64,last_used:Instant,creation:u64}
impl Worker{
    fn spawn(config:&Config,cancel:&dyn Fn()->bool)->io::Result<Self>{
        if cancel(){return Err(io::Error::new(io::ErrorKind::Interrupted,"icon request cancelled"));}
        reap_retired();let permit=Permit::acquire()?;
        real_ancestors(&config.root)?;fs::create_dir_all(&config.root)?;real_ancestors(&config.root)?;
        let session=format!("{}-{}-{:x}",std::process::id(),NEXT.fetch_add(1,Ordering::Relaxed),SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos());
        let workspace=config.root.join(format!("icon-worker-{session}"));fs::create_dir(&workspace)?;
        let created=match process_created(unsafe{GetCurrentProcess()}){Ok(value)=>value,Err(error)=>{cleanup(&workspace);return Err(error)}};
        let owner=Owner{schema:1,parent:std::process::id(),created,session};
        let mut spawned=None;
        let prepared=(||->io::Result<(Job,u64)>{
            atomic_write(&workspace,"owner.json",&serde_json::to_vec(&owner).map_err(io::Error::other)?)?;
            let mut command=Command::new(&config.executable);
            if let Some(prefix)=&config.prefix{command.args(prefix).arg("--color=never").env(TEST_REQUEST,&workspace);}else{command.arg(FLAG).arg(&workspace);}
            command.creation_flags(CREATE_NO_WINDOW.0).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            spawned=Some(command.spawn()?);let child=spawned.as_ref().unwrap();
            let job=Job(unsafe{CreateJobObjectW(None,PCWSTR::null())}.map_err(windows_error)?);
            let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE|JOB_OBJECT_LIMIT_PROCESS_MEMORY|JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
            limits.ProcessMemoryLimit=192*1024*1024;
            unsafe{SetInformationJobObject(job.0,JobObjectExtendedLimitInformation,(&limits as*const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),std::mem::size_of_val(&limits)as u32)
                .and_then(|_|AssignProcessToJobObject(job.0,HANDLE(child.as_raw_handle())))}.map_err(windows_error)?;
            let creation=process_created(HANDLE(child.as_raw_handle()))?;
            atomic_write(&workspace,"worker.json",&serde_json::to_vec(&serde_json::json!({"schema":1,"pid":child.id(),"created":creation,"parent":owner.parent,"session":owner.session,"purpose":"Shell icon or thumbnail extraction"})).map_err(io::Error::other)?)?;
            Ok((job,creation))
        })();
        let(job,creation)=match prepared{Ok(value)=>value,Err(error)=>{
            if let Some(mut child)=spawned.take(){
                let _=child.kill();
                if unsafe{WaitForSingleObject(HANDLE(child.as_raw_handle()),1000)}!=WAIT_OBJECT_0{
                    CLEANUP_ERRORS.fetch_add(1,Ordering::Relaxed);
                    if let Ok(bytes)=serde_json::to_vec(&serde_json::json!({"schema":1,"pid":child.id(),"created":process_created(HANDLE(child.as_raw_handle())).ok(),"parent":owner.parent,"session":owner.session,"purpose":"Shell extraction startup retirement","state":"exit-unconfirmed"})){
                        let _=atomic_write(&workspace,"worker.json",&bytes);
                    }
                    RETIRED.get_or_init(Default::default).lock().unwrap_or_else(|error|error.into_inner()).push(Retired{child,workspace,_permit:permit});
                    return Err(error);
                }
                let _=child.try_wait();
            }
            cleanup(&workspace);return Err(error)
        }};
        let child=spawned.take().expect("successful icon worker spawn owns its process");
        let mut worker=Self{child:Some(child),job:Some(job),permit:Some(permit),workspace,owner,serial:0,last_used:Instant::now(),creation};
        let deadline=Instant::now()+START_TIMEOUT;
        loop{
            if cancel(){return Err(io::Error::new(io::ErrorKind::Interrupted,"icon request cancelled"));}
            if let Ok(bytes)=bounded_read(&worker.workspace.join("ready.json"),4096){
                let ready:Ready=serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if ready.schema!=1||ready.session!=worker.owner.session||Some(ready.pid)!=worker.child.as_ref().map(Child::id){return Err(invalid("icon worker handshake mismatch"));}
                return Ok(worker);
            }
            if worker.exited()?{return Err(io::Error::other("icon worker exited during startup"));}
            if Instant::now()>=deadline{return Err(io::Error::new(io::ErrorKind::TimedOut,"icon worker startup timed out"));}
            thread::sleep(Duration::from_millis(4));
        }
    }
    fn exited(&mut self)->io::Result<bool>{self.child.as_mut().map_or(Ok(true),|child|child.try_wait().map(|status|status.is_some()))}
    fn finish(&mut self){
        let mut stopped=true;
        if let Some(mut child)=self.child.take(){
            // A concrete owned process handle and creation identity are retained;
            // never enumerate or terminate unrelated extractor/server processes.
            if process_created(HANDLE(child.as_raw_handle())).ok()==Some(self.creation)&&child.try_wait().ok().flatten().is_none(){let _=child.kill();}
            self.job.take();
            if unsafe{WaitForSingleObject(HANDLE(child.as_raw_handle()),1000)}==WAIT_OBJECT_0{let _=child.try_wait();}
            else{stopped=false;CLEANUP_ERRORS.fetch_add(1,Ordering::Relaxed);
                if let Some(permit)=self.permit.take(){RETIRED.get_or_init(Default::default).lock().unwrap_or_else(|error|error.into_inner()).push(Retired{child,workspace:self.workspace.clone(),_permit:permit});}
            }
        }
        if stopped{cleanup(&self.workspace);self.permit.take();}
    }
    pub(super) fn expired(&self)->bool{self.last_used.elapsed()>=IDLE}
    pub(super) fn extract(slot:&mut Option<Self>,config:&Config,request:&IconRequest,thumbnail:bool,cancel:&dyn Fn()->bool)->Option<(HBITMAP,BitmapAlpha)>{
        if slot.as_ref().is_some_and(Self::expired){slot.take();}
        if slot.is_none(){match Self::spawn(config,cancel){Ok(worker)=>*slot=Some(worker),Err(_)=>return None}}
        let outcome=slot.as_mut().unwrap().exchange(request,thumbnail,cancel);
        match outcome{
            Ok(image)=>image,
            Err(error)=>{if error.kind()==io::ErrorKind::TimedOut{TIMEOUTS.fetch_add(1,Ordering::Relaxed);}else if error.kind()==io::ErrorKind::Interrupted{CANCELLED.fetch_add(1,Ordering::Relaxed);}slot.take();None},
        }
    }
    fn exchange(&mut self,request:&IconRequest,thumbnail:bool,cancel:&dyn Fn()->bool)->io::Result<Option<(HBITMAP,BitmapAlpha)>>{
        if cancel(){return Err(io::Error::new(io::ErrorKind::Interrupted,"icon request cancelled"));}
        self.serial=self.serial.wrapping_add(1).max(1);
        let value=Request{schema:1,session:self.owner.session.clone(),serial:self.serial,token:request.token,size:request.size,thumbnail,pidl:request.pidl.clone()};
        let bytes=serde_json::to_vec(&value).map_err(io::Error::other)?;
        if bytes.len()as u64>MAX_REQUEST{return Err(invalid("icon request exceeds bound"));}
        // The preceding response has already been consumed. Remove it before
        // advancing the request so a slow decoder does not make the parent
        // reread an old multi-megabyte bitmap on every polling interval.
        let response=self.workspace.join("response.bin");
        match fs::symlink_metadata(&response){
            Ok(metadata)if metadata.is_file()&&metadata.file_attributes()&0x400==0=>fs::remove_file(&response)?,
            Ok(_)=>return Err(invalid("invalid existing icon response")),
            Err(error)if error.kind()==io::ErrorKind::NotFound=>{},Err(error)=>return Err(error),
        }
        atomic_write(&self.workspace,"request.json",&bytes)?;
        let deadline=Instant::now()+if thumbnail{THUMBNAIL_TIMEOUT}else{ICON_TIMEOUT};
        loop{
            if cancel(){return Err(io::Error::new(io::ErrorKind::Interrupted,"icon request cancelled"));}
            if let Ok(bytes)=bounded_read(&self.workspace.join("response.bin"),MAX_READY_BYTES+HEADER as u64){
                if bytes.len()>=HEADER&&&bytes[..8]==MAGIC{
                    let serial=u64::from_le_bytes(bytes[8..16].try_into().unwrap());
                    if serial==self.serial{self.last_used=Instant::now();return decode(&bytes,request.token);}
                    if serial>self.serial{return Err(invalid("future icon response"));}
                }else{return Err(invalid("invalid icon response header"));}
            }
            if self.exited()?{return Err(io::Error::other("icon worker exited during extraction"));}
            if Instant::now()>=deadline{return Err(io::Error::new(io::ErrorKind::TimedOut,"icon extraction timed out"));}
            thread::sleep(Duration::from_millis(4));
        }
    }
}
impl Drop for Worker{fn drop(&mut self){self.finish();}}

fn encode(serial:u64,token:u64,image:Option<(u32,u32,Vec<u8>)>)->io::Result<Vec<u8>>{
    let mut bytes=Vec::new();bytes.extend(MAGIC);bytes.extend(serial.to_le_bytes());bytes.extend(token.to_le_bytes());
    let(width,height,pixels)=image.unwrap_or_default();
    if width>2048||height>2048||(width==0)!=(height==0){return Err(invalid("invalid extracted icon dimensions"));}
    let count=u64::from(width)*u64::from(height)*4;
    if count>MAX_READY_BYTES||count!=pixels.len()as u64{return Err(invalid("invalid extracted icon dimensions"));}
    bytes.extend(width.to_le_bytes());bytes.extend(height.to_le_bytes());bytes.extend(pixels);Ok(bytes)
}
fn decode(bytes:&[u8],token:u64)->io::Result<Option<(HBITMAP,BitmapAlpha)>>{
    if bytes.len()<HEADER||&bytes[..8]!=MAGIC||u64::from_le_bytes(bytes[16..24].try_into().unwrap())!=token{return Err(invalid("icon response identity mismatch"));}
    let width=u32::from_le_bytes(bytes[24..28].try_into().unwrap());let height=u32::from_le_bytes(bytes[28..32].try_into().unwrap());
    if width>2048||height>2048||(width==0)!=(height==0){return Err(invalid("icon response dimensions mismatch"));}
    let size=u64::from(width)*u64::from(height)*4;
    if size>MAX_READY_BYTES||size+HEADER as u64!=bytes.len()as u64{return Err(invalid("icon response dimensions mismatch"));}
    if size==0{return Ok(None);}
    let info=BITMAPINFO{bmiHeader:BITMAPINFOHEADER{biSize:std::mem::size_of::<BITMAPINFOHEADER>()as u32,biWidth:width as i32,biHeight:-(height as i32),biPlanes:1,biBitCount:32,biCompression:BI_RGB.0,..Default::default()},..Default::default()};
    let mut bits=std::ptr::null_mut();let bitmap=unsafe{CreateDIBSection(None,&info,DIB_RGB_COLORS,&mut bits,None,0)}.map_err(windows_error)?;
    if bits.is_null(){let _=unsafe{DeleteObject(bitmap.into())};return Err(invalid("icon bitmap allocation missing pixels"));}
    unsafe{std::ptr::copy_nonoverlapping(bytes[HEADER..].as_ptr(),bits.cast::<u8>(),size as usize);}
    Ok(Some((bitmap,BitmapAlpha::Premultiplied)))
}
fn extract_pixels(gfx:&Gfx,request:&Request)->Option<(u32,u32,Vec<u8>)>{
    let input=IconRequest{key:String::new(),pidl:request.pidl.clone(),size:request.size,thumbnail:request.thumbnail,token:request.token};
    let(bitmap,alpha)=load(&input)?;
    struct Bitmap(HBITMAP);impl Drop for Bitmap{fn drop(&mut self){let _=unsafe{DeleteObject(self.0.into())};}}
    let bitmap=Bitmap(bitmap);let source=gfx.wic_from_hbitmap(bitmap.0,alpha)?;let(width,height)=Gfx::wic_size(&source);
    if width==0||height==0||width>2048||height>2048{return None;}
    let count=u64::from(width)*u64::from(height)*4;
    if count>MAX_READY_BYTES{return None;}
    let mut pixels=vec![0;count as usize];unsafe{source.CopyPixels(std::ptr::null(),width*4,&mut pixels)}.ok()?;
    Some((width,height,pixels))
}
fn run(workspace:&Path)->io::Result<()>{run_with(workspace,extract_pixels)}
fn run_with(workspace:&Path,mut extract:impl FnMut(&Gfx,&Request)->Option<(u32,u32,Vec<u8>)>)->io::Result<()>{
    // Process-local error handling: a broken extension must not open a crash
    // dialog over the user's desktop while the owned helper is being retired.
    unsafe{SetErrorMode(0x0001|0x0002|0x8000);}
    local_path(workspace)?;real_ancestors(workspace)?;
    let owner:Owner=serde_json::from_slice(&bounded_read(&workspace.join("owner.json"),4096)?).map_err(io::Error::other)?;
    if owner.schema!=1||owner.session.is_empty()||owner.session.len()>128||workspace.file_name()!=Some(std::ffi::OsStr::new(&format!("icon-worker-{}",owner.session))){return Err(invalid("invalid icon worker owner"));}
    let parent=unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION|PROCESS_SYNCHRONIZE,false,owner.parent)}.map_err(windows_error)?;
    struct Parent(HANDLE);impl Drop for Parent{fn drop(&mut self){let _=unsafe{CloseHandle(self.0)};}}let parent=Parent(parent);
    if process_created(parent.0)?!=owner.created{return Err(invalid("icon parent identity changed"));}
    unsafe{CoInitializeEx(None,COINIT_APARTMENTTHREADED|COINIT_DISABLE_OLE1DDE)}.ok().map_err(windows_error)?;
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}let _apartment=Apartment;
    let gfx=Gfx::new("Segoe UI").map_err(windows_error)?;
    atomic_write(workspace,"ready.json",&serde_json::to_vec(&Ready{schema:1,session:owner.session.clone(),pid:std::process::id()}).map_err(io::Error::other)?)?;
    let mut serial=0;let mut last=Instant::now();let mut stamp=None;
    while unsafe{WaitForSingleObject(parent.0,0)}!=WAIT_OBJECT_0&&last.elapsed()<IDLE+Duration::from_secs(2){
        let request_path=workspace.join("request.json");
        let current=fs::symlink_metadata(&request_path).ok().map(|metadata|(metadata.len(),metadata.last_write_time(),metadata.creation_time(),metadata.file_attributes()));
        if current.is_some()&&current==stamp{thread::sleep(Duration::from_millis(4));continue;}
        match bounded_read(&request_path,MAX_REQUEST){
            Ok(bytes)=>{
                let request:Request=serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if request.schema!=1||request.session!=owner.session||request.token==0||!(1..=1024).contains(&request.size)||!super::super::pidl::valid(&request.pidl){return Err(invalid("invalid icon request"));}
                if request.serial>serial{serial=request.serial;let pixels=extract(&gfx,&request);atomic_write(workspace,"response.bin",&encode(serial,request.token,pixels)?)?;last=Instant::now();}
                else if request.serial<serial{return Err(invalid("icon request serial regressed"));}
                stamp=current;
            },
            Err(error)if error.kind()==io::ErrorKind::NotFound=>{},Err(error)=>return Err(error),
        }
        thread::sleep(Duration::from_millis(4));
    }Ok(())
}
pub fn maybe_handle(args:&[OsString])->Option<io::Result<()>>{
    if args.first().is_none_or(|arg|arg!=FLAG){return None;}
    Some(if args.len()==2{run(Path::new(&args[1]))}else{Err(invalid("icon worker requires one workspace"))})
}

#[cfg(test)]mod tests{
    use super::*;
    use std::os::windows::io::{FromRawHandle,OwnedHandle};
    use windows::Win32::{Foundation::{DuplicateHandle,DUPLICATE_SAME_ACCESS},UI::WindowsAndMessaging::*};
    fn prefix(name:&str)->Vec<OsString>{vec![format!("shell::icons::worker::tests::{name}").into(),"--exact".into(),"--ignored".into(),"--nocapture".into()]}
    fn fixture_root(name:&str)->io::Result<PathBuf>{
        let output=std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").map(PathBuf::from).ok_or_else(||invalid("ZSDESK_NATIVE_TEST_OUTPUT is required"))?;
        local_path(&output)?;real_ancestors(&output)?;
        let root=output.join(name);fs::create_dir_all(&root)?;Ok(root)
    }
    fn config(root:&Path,entry:&str)->io::Result<Config>{Ok(Config{executable:std::env::current_exe()?,root:root.join("workers"),prefix:Some(prefix(entry))})}
    fn request(token:u64,thumbnail:bool)->IconRequest{IconRequest{key:"owned-fixture".into(),pidl:vec![0,0],size:32,thumbnail,token}}
    fn duplicate(child:&Child)->io::Result<OwnedHandle>{
        let mut value=HANDLE::default();unsafe{DuplicateHandle(GetCurrentProcess(),HANDLE(child.as_raw_handle()),GetCurrentProcess(),&mut value,0,false,DUPLICATE_SAME_ACCESS)}.map_err(windows_error)?;
        Ok(unsafe{OwnedHandle::from_raw_handle(value.0)})
    }
    fn no_workers(root:&Path)->bool{fs::read_dir(root.join("workers")).map_or(true,|mut entries|entries.next().is_none())}
    #[test]fn icon_protocol_rejects_wrong_tokens_dimensions_and_trailing_data(){
        let valid=encode(1,7,Some((1,1,vec![64,0,0,128]))).unwrap();
        assert!(decode(&valid,8).is_err());
        let mut extra=valid.clone();extra.push(0);assert!(decode(&extra,7).is_err());
        let mut huge=valid.clone();huge[24..28].copy_from_slice(&u32::MAX.to_le_bytes());assert!(decode(&huge,7).is_err());
        assert!(decode(&encode(2,7,None).unwrap(),7).unwrap().is_none());
        let(bitmap,alpha)=decode(&valid,7).unwrap().unwrap();
        struct Bitmap(HBITMAP);impl Drop for Bitmap{fn drop(&mut self){let _=unsafe{DeleteObject(self.0.into())};}}let _bitmap=Bitmap(bitmap);
        assert!(matches!(alpha,BitmapAlpha::Premultiplied));
        let mut section=super::super::DIBSECTION::default();
        unsafe{super::super::GetObjectW(bitmap.into(),std::mem::size_of_val(&section)as i32,Some((&mut section as*mut super::super::DIBSECTION).cast()))};
        assert!(!section.dsBm.bmBits.is_null());assert_eq!(unsafe{std::slice::from_raw_parts(section.dsBm.bmBits.cast::<u8>(),4)},&[64,0,0,128]);
    }
    #[test]#[ignore="Owned hidden image worker endpoint"]
    fn native_icon_worker_entry(){run_test_entry().unwrap();}
    #[test]#[ignore="Owned hidden stalled-provider endpoint"]
    fn native_icon_worker_stall_entry(){
        let workspace=std::env::var_os(TEST_REQUEST).expect("workspace");
        run_with(Path::new(&workspace),|_,_|{thread::sleep(Duration::from_secs(60));None}).unwrap();
    }
    #[test]#[ignore="Owned hidden thumbnail-only stall endpoint"]
    fn native_icon_worker_thumbnail_stall_entry(){
        let workspace=std::env::var_os(TEST_REQUEST).expect("workspace");
        run_with(Path::new(&workspace),|gfx,request|if request.thumbnail{thread::sleep(Duration::from_secs(60));None}else{extract_pixels(gfx,request)}).unwrap();
    }
    #[test]#[ignore="Owned process timeout/cancellation, no desktop input"]
    fn native_icon_worker_timeout_cancel_and_recovery()->io::Result<()>{
        let root=fixture_root("icon-worker-contract")?;let before_cleanup=CLEANUP_ERRORS.load(Ordering::Relaxed);
        let bad=config(&root,"native_icon_worker_stall_entry")?;
        let mut worker=Some(Worker::spawn(&bad,&||false)?);
        let held=duplicate(worker.as_ref().unwrap().child.as_ref().unwrap())?;
        let id=worker.as_ref().unwrap().child.as_ref().unwrap().id();let creation=worker.as_ref().unwrap().creation;
        let start=Instant::now();assert!(Worker::extract(&mut worker,&bad,&request(1,false),false,&||false).is_none());
        assert!(start.elapsed()<ICON_TIMEOUT+Duration::from_secs(2));assert!(worker.is_none());
        assert_eq!(unsafe{WaitForSingleObject(HANDLE(held.as_raw_handle()),0)},WAIT_OBJECT_0);assert!(no_workers(&root));
        let mut worker=Some(Worker::spawn(&bad,&||false)?);let held_cancel=duplicate(worker.as_ref().unwrap().child.as_ref().unwrap())?;
        let start=Instant::now();assert!(Worker::extract(&mut worker,&bad,&request(2,false),false,&||start.elapsed()>Duration::from_millis(80)).is_none());
        assert!(start.elapsed()<Duration::from_secs(2));assert_eq!(unsafe{WaitForSingleObject(HANDLE(held_cancel.as_raw_handle()),0)},WAIT_OBJECT_0);assert!(no_workers(&root));
        let good=config(&root,"native_icon_worker_entry")?;let mut worker=None;
        let(bitmap,_)=Worker::extract(&mut worker,&good,&request(3,false),false,&||false).ok_or_else(||invalid("real Shell icon extraction failed"))?;
        let _=unsafe{DeleteObject(bitmap.into())};let first=worker.as_ref().unwrap().child.as_ref().unwrap().id();
        let(bitmap,_)=Worker::extract(&mut worker,&good,&request(4,false),false,&||false).ok_or_else(||invalid("reused worker extraction failed"))?;
        let _=unsafe{DeleteObject(bitmap.into())};assert_eq!(worker.as_ref().unwrap().child.as_ref().unwrap().id(),first);
        drop(worker);assert_eq!(pending_workers(),0);assert!(no_workers(&root));assert_eq!(CLEANUP_ERRORS.load(Ordering::Relaxed),before_cleanup);
        fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&serde_json::json!({"state":"passed","owned_timed_out_pid":id,"owned_creation":creation,"timeout_reaped":true,"cancel_reaped":true,"workspace_removed":true,"real_shell_icon":true,"reused_process":true,"desktop_input":false})).map_err(io::Error::other)?)?;
        Ok(())
    }
    #[test]#[ignore="Independent icon and thumbnail lanes using owned hidden workers"]
    fn native_slow_thumbnail_does_not_block_icons_or_shutdown()->io::Result<()>{
        let root=fixture_root("icon-lanes")?;let before_cleanup=CLEANUP_ERRORS.load(Ordering::Relaxed);
        let config=config(&root,"native_icon_worker_thumbnail_stall_entry")?;
        let hwnd=unsafe{CreateWindowExW(WINDOW_EX_STYLE::default(),windows::core::w!("STATIC"),None,WS_POPUP,0,0,1,1,None,None,None,None)}.map_err(windows_error)?;
        struct Window(windows::Win32::Foundation::HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let _window=Window(hwnd);
        let mut loader=super::super::IconLoader::spawn_with_config(hwnd,WM_APP+107,Some(config));
        assert!(loader.request(request(1,true)));
        let deadline=Instant::now()+START_TIMEOUT+ICON_TIMEOUT+Duration::from_secs(1);let mut provisional=false;
        while Instant::now()<deadline&&!provisional{
            for result in loader.take_ready(){assert_eq!(result.token,1);assert!(!result.bitmap.is_invalid());assert!(!result.final_result);provisional=true;}
            thread::sleep(Duration::from_millis(4));
        }
        assert!(provisional,"ordinary icon was not delivered before its thumbnail");
        let start=Instant::now();assert!(loader.request(request(2,false)));let mut second=false;
        while start.elapsed()<ICON_TIMEOUT+Duration::from_secs(1)&&!second{
            for result in loader.take_ready(){assert_ne!(result.token,1,"stalled thumbnail completed unexpectedly");if result.token==2{assert!(result.final_result&&!result.bitmap.is_invalid());second=true;}}
            thread::sleep(Duration::from_millis(4));
        }
        assert!(second,"thumbnail lane blocked an ordinary icon");
        let icon_elapsed=start.elapsed().as_millis();loader.retain_requests(&std::collections::HashSet::new());
        let start=Instant::now();loader.shutdown();assert!(start.elapsed()<Duration::from_secs(2));
        assert_eq!(pending_workers(),0);assert!(no_workers(&root));assert!(loader.take_ready().is_empty());
        assert_eq!(CLEANUP_ERRORS.load(Ordering::Relaxed),before_cleanup);
        fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&serde_json::json!({"state":"passed","provisional_icon":true,"ordinary_icon_while_thumbnail_blocked_ms":icon_elapsed,"bounded_shutdown":true,"no_stale_results":true,"no_live_owned_workers":true,"desktop_input":false})).map_err(io::Error::other)?)?;
        Ok(())
    }
}
