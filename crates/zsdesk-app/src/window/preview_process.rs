//! Disposable preview host. The file-manager STA never calls a document decoder.
use std::{cell::{Cell,RefCell},fs,path::{Path,PathBuf},process::{Child,Command,Stdio},sync::atomic::{AtomicU64,Ordering},time::{Duration,Instant}};
use std::os::windows::{process::CommandExt,io::AsRawHandle,fs::MetadataExt};
use anyhow::{bail,Context,Result};
use serde::{Deserialize,Serialize};
use windows::{core::PCWSTR,Win32::{Foundation::{CloseHandle,HANDLE,HWND,LPARAM,POINT,RECT,WPARAM},
    System::{JobObjects::*,Ole::{OleInitialize,OleUninitialize},Threading::CREATE_NO_WINDOW},UI::WindowsAndMessaging::*}};
use zsdesk_core::model::explorer::PreviewProviderPreference;
#[path="preview_selection.rs"]
pub(super) mod selection;
use selection::{ViewIdentity,ViewSnapshot};

pub(super) const TIMER:usize=8;
const LOAD_TIMEOUT:Duration=Duration::from_secs(30);
static SERIAL:AtomicU64=AtomicU64::new(1);
#[derive(Clone,Serialize,Deserialize)]
struct Request{parent:u32,owner:isize,path:PathBuf,provider:PreviewProviderPreference,dark:bool,hover:bool,generation:u64,close:bool,embedded:Option<[i32;4]>}
#[derive(Serialize,Deserialize)]
struct Status{pid:u32,hwnd:isize,generation:u64,ready:bool,kind:String,notice:String}

fn write_json(path:&Path,value:&impl Serialize)->Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING};
    let temporary=path.with_extension("tmp");fs::write(&temporary,serde_json::to_vec(value)?)?;
    let from:Vec<_>=temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to:Vec<_>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe{MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_REPLACE_EXISTING)}?;Ok(())
}
pub(super) fn cleanup(workspace:&Path){
    // This unique directory contains only this preview's protocol files/PDFs.
    if workspace.file_name().is_none_or(|name|!name.to_string_lossy().starts_with("preview-")){return;}
    // A breakaway WPS renderer owns the only document-cleanup path. Its
    // delete-on-close lease survives disposal of the visible preview helper.
    if workspace.join("wps-active.lock").exists(){return;}
    if let Ok(items)=fs::read_dir(workspace){for item in items.flatten(){
        let name=item.file_name();let name=name.to_string_lossy();
        let protocol= ["wps-request-","wps-result-","wps-cancel-"].iter().any(|prefix|
            name.strip_prefix(prefix).is_some_and(|tail|{
                let id=tail.strip_suffix(".json").or_else(||tail.strip_suffix(".tmp")).unwrap_or(tail);
                !id.is_empty()&&id.bytes().all(|byte|byte.is_ascii_digit())
            }));
        if !["request.json","request.tmp","status.json","status.tmp","worker.stderr.log","wps-lifecycle.json","wps-worker.json"].contains(&name.as_ref())
            && !protocol && !(name.starts_with("wps-")&&name.ends_with(".pdf")){continue;}
        if item.path().symlink_metadata().is_ok_and(|metadata|metadata.is_file()&&metadata.file_attributes()&0x400==0){let _=fs::remove_file(item.path());}
    }}
    let _=fs::remove_dir(workspace);
}

pub(super) struct PreviewProcess {
    child:RefCell<Option<Child>>,job:Option<usize>,workspace:PathBuf,request:Request,
    hwnd:Cell<HWND>,started:Instant,ready:Cell<bool>,selection:Option<ViewIdentity>,
}
impl PreviewProcess {
    pub(super) fn open_with_options(owner:HWND,path:&Path,provider:PreviewProviderPreference,dark:bool,hover:bool)->Result<Self>{
        Self::open_impl(owner,path,provider,dark,hover,None)
    }
    fn open_impl(owner:HWND,path:&Path,provider:PreviewProviderPreference,dark:bool,hover:bool,embedded:Option<[i32;4]>)->Result<Self>{
        let root=std::env::var_os("ZSDESK_PREVIEW_ROOT").map(PathBuf::from)
            .unwrap_or_else(||dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join(crate::edition::profile_name()).join("previews"));
        if !root.is_absolute(){bail!("预览缓存目录必须是绝对路径");}
        fs::create_dir_all(&root)?;
        let workspace=root.join(format!("preview-{}-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()));
        fs::create_dir(&workspace)?;
        let request=Request{parent:std::process::id(),owner:owner.0 as isize,path:path.to_path_buf(),provider,dark,hover,generation:1,close:false,embedded};
        // Errors before spawning still own this newly created protocol directory.
        let prepared=(||->Result<_>{
            write_json(&workspace.join("request.json"),&request)?;
            Ok((std::env::current_exe()?,fs::File::create(workspace.join("worker.stderr.log"))?))
        })();
        let (executable,stderr)=match prepared{Ok(prepared)=>prepared,Err(error)=>{cleanup(&workspace);return Err(error);}};
        let mut command=Command::new(executable);
        command.arg("--preview-worker").arg(workspace.join("request.json"))
            .env("ZSDESK_PREVIEW_WORKSPACE",&workspace).creation_flags(CREATE_NO_WINDOW.0)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
        let mut child=match command.spawn(){Ok(child)=>child,Err(error)=>{cleanup(&workspace);return Err(error.into());}};
        let job=unsafe{CreateJobObjectW(None,PCWSTR::null())};
        let job=match job{Ok(job)=>job,Err(error)=>{let _=child.kill();let _=child.wait();cleanup(&workspace);return Err(error.into());}};
        let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE|JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let attached=unsafe{SetInformationJobObject(job,JobObjectExtendedLimitInformation,(&limits as*const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),std::mem::size_of_val(&limits)as u32)
            .and_then(|_|AssignProcessToJobObject(job,HANDLE(child.as_raw_handle())))};
        if let Err(error)=attached{let _=child.kill();let _=child.wait();let _=unsafe{CloseHandle(job)};cleanup(&workspace);return Err(error.into());}
        unsafe{SetTimer(Some(owner),TIMER,100,None);}
        Ok(Self{child:RefCell::new(Some(child)),job:Some(job.0 as usize),workspace,request,hwnd:Cell::new(HWND::default()),started:Instant::now(),ready:Cell::new(false),selection:None})
    }
    pub(super) fn is_open(&self)->bool{
        self.child.borrow_mut().as_mut().is_some_and(|child|child.try_wait().ok().flatten().is_none())
    }
    pub(super) fn is_hover(&self)->bool{self.request.hover}
    pub(super) fn contains_point(&self,point:POINT)->bool{
        let mut bounds=RECT::default();let hwnd=self.hwnd.get();
        !hwnd.0.is_null()&&unsafe{GetWindowRect(hwnd,&mut bounds)}.is_ok()&&point.x>=bounds.left&&point.x<bounds.right&&point.y>=bounds.top&&point.y<bounds.bottom
    }
    pub(super) fn owns_window(&self,hwnd:HWND)->bool{hwnd==self.hwnd.get()&&!hwnd.0.is_null()||unsafe{IsChild(self.hwnd.get(),hwnd)}.as_bool()}
    pub(super) fn pre_translate(&mut self,_message:&MSG)->bool{false}
    fn layout(&mut self,bounds:[i32;4]){
        let hwnd=self.hwnd.get();if hwnd.0.is_null(){return;}
        let mut current=RECT::default();let _=unsafe{GetWindowRect(hwnd,&mut current)};
        let mut origin=POINT{x:current.left,y:current.top};let _=unsafe{windows::Win32::Graphics::Gdi::ScreenToClient(HWND(self.request.owner as*mut _),&mut origin)};
        if [origin.x,origin.y,current.right-current.left,current.bottom-current.top]!=bounds{
            let _=unsafe{SetWindowPos(hwnd,None,bounds[0],bounds[1],bounds[2],bounds[3],SWP_NOACTIVATE|SWP_NOZORDER|SWP_ASYNCWINDOWPOS)};
        }
    }
    pub(super) fn set_dark(&mut self,dark:bool)->Result<()>{
        if self.request.dark==dark{return Ok(());}
        if self.child.borrow().is_none(){self.request.dark=dark;return Ok(());}
        let previous=self.request.dark;self.request.dark=dark;
        if let Err(error)=write_json(&self.workspace.join("request.json"),&self.request){self.request.dark=previous;return Err(error);}Ok(())
    }
    pub(super) fn update(&mut self,path:&Path)->Result<()>{
        if self.request.path==path{return Ok(());}
        let mut next=self.request.clone();next.path=path.into();next.generation=next.generation.wrapping_add(1);
        // A failed IPC write must not acknowledge a selection the helper never saw.
        write_json(&self.workspace.join("request.json"),&next)?;
        self.request=next;self.started=Instant::now();self.ready.set(false);Ok(())
    }
    pub(super) fn poll(&mut self)->Option<String>{
        let exited=self.child.borrow_mut().as_mut().and_then(|child|child.try_wait().ok().flatten());
        if let Some(exit)=exited {
            // The visible worker can close itself. Reap its handles and files
            // now; a completed pane keeps only its request as a retry barrier.
            self.release_exited();
            if !exit.success(){
                super::diagnostics::record("preview","worker-exit",exit.code());
                return Some(format!("预览宿主异常退出（{exit}），文件管理窗口可继续使用。"));
            }
            return None;
        }
        if self.child.borrow().is_none(){return None;}
        if let Ok(bytes)=fs::read(self.workspace.join("status.json")){
            if bytes.len()<65536{if let Ok(status)=serde_json::from_slice::<Status>(&bytes){
                if self.child.borrow().as_ref().is_some_and(|child|child.id()==status.pid){
                    let window=HWND(status.hwnd as*mut _);let mut pid=0;unsafe{GetWindowThreadProcessId(window,Some(&mut pid));}
                    if pid==status.pid {self.hwnd.set(window);}
                    if status.generation==self.request.generation {self.ready.set(status.ready);}
                }
            }}
        }
        if self.is_open()&&!self.ready.get()&&self.started.elapsed()>LOAD_TIMEOUT{
            super::diagnostics::record("preview","load-timeout",Some(0x800705b4u32 as i32));
            self.close();return Some("预览加载超时，已关闭预览宿主；文件管理窗口可继续使用。".into());
        }
        None
    }
    fn release_exited(&mut self){
        self.child.get_mut().take();
        self.hwnd.set(HWND::default());self.ready.set(false);
        if let Some(job)=self.job.take(){let _=unsafe{CloseHandle(HANDLE(job as*mut _))};}
        cleanup(&self.workspace);
    }
    fn replace_pane(&self,provider:PreviewProviderPreference,path:&Path)->bool{
        self.request.provider!=provider||(!self.is_open()&&self.request.path!=path)
    }
    pub(super) fn close(&mut self){
        let Some(mut child)=self.child.borrow_mut().take() else{return;};
        self.request.close=true;let _=write_json(&self.workspace.join("request.json"),&self.request);
        let window=self.hwnd.replace(HWND::default());
        if !window.0.is_null(){let _=unsafe{PostMessageW(Some(window),WM_CLOSE,WPARAM(0),LPARAM(0))};}
        let job=self.job.take();let workspace=self.workspace.clone();
        // Only this owned helper is terminated after its grace period; WPS and
        // shared system preview-host processes are never killed by image name.
        std::thread::spawn(move||{
            let deadline=Instant::now()+Duration::from_secs(5);
            while child.try_wait().ok().flatten().is_none()&&Instant::now()<deadline{std::thread::sleep(Duration::from_millis(25));}
            if child.try_wait().ok().flatten().is_none(){let _=child.kill();}
            let _=child.wait();if let Some(job)=job{let _=unsafe{CloseHandle(HANDLE(job as*mut _))};}cleanup(&workspace);
        });
    }
}
impl Drop for PreviewProcess{fn drop(&mut self){self.close();}}

pub(super) fn poll_previews(quick:&mut Option<PreviewProcess>,pane:&mut Option<PreviewProcess>)->Vec<String>{
    let mut errors=Vec::new();
    for preview in [quick.as_mut(),pane.as_mut()].into_iter().flatten(){
        if let Some(error)=preview.poll(){errors.push(error);}
    }
    if quick.as_ref().is_some_and(|preview|preview.child.borrow().is_none()){*quick=None;}
    errors
}
pub(super) fn needs_timer(quick:&Option<PreviewProcess>,pane:&Option<PreviewProcess>)->bool{
    [quick.as_ref(),pane.as_ref()].into_iter().flatten().any(|preview|preview.child.borrow().is_some())
}

pub(crate) fn run_if_requested()->Option<Result<()>>{
    if let Some(result)=super::wps_preview::run_worker_if_requested(){return Some(result);}
    if std::env::args_os().nth(1).as_deref()!=Some(std::ffi::OsStr::new("--preview-worker")){return None;}
    Some((||{
        let path=PathBuf::from(std::env::args_os().nth(2).context("缺少预览请求")?);
        if std::env::args_os().count()!=3||!path.is_absolute(){bail!("预览请求无效");}
        unsafe{OleInitialize(None)}?;
        let result=run_worker(&path);
        unsafe{OleUninitialize();}result
    })())
}
fn read_request(path:&Path)->Result<Request>{let bytes=fs::read(path)?;if bytes.len()>65536{bail!("预览请求过长");}Ok(serde_json::from_slice(&bytes)?)}
fn owner_alive(request:&Request)->bool{
    let mut pid=0;let hwnd=HWND(request.owner as*mut _);unsafe{GetWindowThreadProcessId(hwnd,Some(&mut pid));}
    pid==request.parent&&unsafe{IsWindow(Some(hwnd))}.as_bool()
}
fn run_worker(path:&Path)->Result<()>{
    let mut request=read_request(path)?;if !owner_alive(&request)||request.close{return Ok(());}
    let owner=HWND(request.owner as*mut _);
    let bounds=request.embedded.map(|r|RECT{left:r[0],top:r[1],right:r[0]+r[2],bottom:r[1]+r[3]});
    let mut preview=super::quick_preview::QuickPreview::create_worker(owner,request.provider,request.dark,request.hover,bounds)?;
    let status_path=path.with_file_name("status.json");
    let load=|preview:&mut super::quick_preview::QuickPreview,request:&Request|->Result<()>{
        let hwnd=preview.window();
        write_json(&status_path,&Status{pid:std::process::id(),hwnd:hwnd.0 as isize,generation:request.generation,ready:false,kind:"loading".into(),notice:String::new()})?;
        preview.update(&request.path)?;
        let (kind,notice)=preview.status();
        write_json(&status_path,&Status{pid:std::process::id(),hwnd:hwnd.0 as isize,generation:request.generation,ready:true,kind,notice})
    };
    load(&mut preview,&request)?;
    unsafe{SetTimer(Some(preview.window()),99,100,None);}
    let mut message=MSG::default();
    while preview.is_open()&&unsafe{GetMessageW(&mut message,None,0,0)}.0>0{
        if request.embedded.is_some() && matches!(message.message,WM_KEYDOWN|WM_SYSKEYDOWN){
            use windows::Win32::UI::Input::KeyboardAndMouse::*;
            let ctrl=unsafe{GetKeyState(VK_CONTROL.0 as i32)}<0;let alt=unsafe{GetKeyState(VK_MENU.0 as i32)}<0;
            let action=match message.wParam.0 as u16{
                key if key==VK_P.0&&alt=>Some(crate::commands::Command::TogglePreview),
                key if key==VK_L.0&&ctrl=>Some(crate::commands::Command::FocusAddress),
                key if key==VK_F11.0=>Some(crate::commands::Command::ToggleFullscreen),
                key if key==VK_F6.0=>Some(if unsafe{GetKeyState(VK_SHIFT.0 as i32)}<0{crate::commands::Command::FocusPrevious}else{crate::commands::Command::FocusNext}),
                _=>None,
            };
            if let Some(action)=action{super::post(owner,super::WM_ACTION,action.id()as usize,0);continue;}
        }
        if message.message==WM_TIMER&&message.wParam.0==99{
            if !owner_alive(&request){break;}
            if let Ok(next)=read_request(path){
                if next.parent!=request.parent||next.owner!=request.owner||next.close{break;}
                if next.dark!=request.dark{preview.set_dark(next.dark);request.dark=next.dark;}
                if next.generation!=request.generation{request=next;load(&mut preview,&request)?;}
            }
            continue;
        }
        if !preview.pre_translate(&message){unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}
    }
    preview.close();Ok(())
}

pub(super) fn selected_preview_request(slot:&super::Slot)->Option<(PathBuf,PreviewProviderPreference,bool)>{
    let snapshot=ViewSnapshot::capture(slot)?;
    let path=snapshot.selected_path(slot)?;
    Some((path,snapshot.provider,snapshot.dark))
}

fn pane_context(slot:&super::Slot)->Option<([i32;4],PreviewProviderPreference,bool)>{
    slot.try_borrow().ok().and_then(|slot|slot.as_ref().and_then(|state|state.preview_rect.map(|rect|
        ([rect.x,rect.y,rect.width,rect.height],state.session.ui.preview_provider,state.dark))))
}
pub(super) fn sync_pane(owner:HWND,slot:&super::Slot,pane:&mut Option<PreviewProcess>){
    let context=pane_context(slot);
    let Some((bounds,provider,dark))=context else{*pane=None;return;};
    let snapshot=ViewSnapshot::capture(slot);
    let identity=snapshot.as_ref().map(|snapshot|snapshot.identity.clone());
    // The 100 ms supervision timer must not continuously enter Shell to fetch
    // the same selection. The native selection revision invalidates this cache.
    let unchanged=pane.as_ref().is_some_and(|pane|pane.selection==identity);
    let path=if unchanged{pane.as_ref().unwrap().request.path.clone()}
        else{snapshot.as_ref().and_then(|snapshot|snapshot.selected_path(slot)).unwrap_or_default()};
    // A COM callback may have switched tabs, moved a pane or closed the window.
    if snapshot.as_ref().is_some_and(|snapshot|!snapshot.current(slot,true))||pane_context(slot)!=context{return;}
    if pane.as_ref().is_some_and(|pane|pane.replace_pane(provider,&path)){*pane=None;}
    if pane.is_none(){
        match PreviewProcess::open_impl(owner,&path,provider,dark,false,Some(bounds)){
            Ok(process)=>*pane=Some(process),
            Err(error)=>if let Ok(mut slot)=slot.try_borrow_mut(){if let Some(state)=slot.as_mut(){state.show_message(format!("无法打开预览窗格：{error}"));}},
        }
    }
    if let Some(pane)=pane.as_mut(){
        if pane.update(&path).is_ok(){pane.selection=identity;}
        let _=pane.set_dark(dark);pane.layout(bounds);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use anyhow::ensure;

    #[link(name="kernel32")]
    unsafe extern "system" {
        fn GetHandleInformation(handle:*mut core::ffi::c_void,flags:*mut u32)->i32;
    }

    fn exited_preview(code:u32,embedded:bool)->Result<PreviewProcess>{
        // These ignored tests create only a hidden exit-code helper, never a
        // preview window or a document handler on the interactive desktop.
        let root=PathBuf::from(std::env::var_os("ZSDESK_PREVIEW_ROOT").context("isolated preview root required")?);
        ensure!(root.is_absolute(),"preview root must be absolute");
        let diagnostics=PathBuf::from(std::env::var_os("ZSDESK_DIAGNOSTICS_ROOT").context("isolated diagnostic root required")?);
        ensure!(diagnostics.is_absolute(),"diagnostic root must be absolute");
        fs::create_dir_all(&root)?;
        let workspace=root.join(format!("preview-exit-test-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&workspace)?;
        let request=Request{parent:std::process::id(),owner:0,path:root.join("selected-document.txt"),
            provider:PreviewProviderPreference::System,dark:false,hover:false,generation:1,close:false,
            embedded:embedded.then_some([0,0,200,200])};
        write_json(&workspace.join("request.json"),&request)?;
        let system=PathBuf::from(std::env::var_os("SystemRoot").context("Windows system directory missing")?).join("System32").join("cmd.exe");
        let mut child=Command::new(system).args(["/d","/c",&format!("exit /b {code}")])
            .creation_flags(CREATE_NO_WINDOW.0).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        child.wait()?;
        let job=unsafe{CreateJobObjectW(None,PCWSTR::null())}?;
        Ok(PreviewProcess{child:RefCell::new(Some(child)),job:Some(job.0 as usize),workspace,request,
            hwnd:Cell::new(HWND::default()),started:Instant::now(),ready:Cell::new(true),selection:None})
    }

    #[test]
    #[ignore="owned hidden helper; requires isolated preview and diagnostic roots"]
    fn native_preview_normal_exit_releases_handles_files_and_timer()->Result<()>{
        let preview=exited_preview(0,false)?;
        let workspace=preview.workspace.clone();
        let process_handle=preview.child.borrow().as_ref().unwrap().as_raw_handle();
        let job_handle=preview.job.unwrap()as*mut core::ffi::c_void;
        let mut quick=Some(preview);let mut pane=None;
        assert!(needs_timer(&quick,&pane),"an unreaped worker still needs supervision");
        assert!(poll_previews(&mut quick,&mut pane).is_empty());
        assert!(quick.is_none());assert!(!needs_timer(&quick,&pane));
        assert!(!workspace.exists(),"normal worker exit left protocol files");
        let mut flags=0;
        assert_eq!(unsafe{GetHandleInformation(process_handle,&mut flags)},0,"child handle stayed open");
        assert_eq!(unsafe{GetHandleInformation(job_handle,&mut flags)},0,"job handle stayed open");
        assert!(poll_previews(&mut quick,&mut pane).is_empty());
        Ok(())
    }

    #[test]
    #[ignore="owned hidden helper; requires isolated preview and diagnostic roots"]
    fn native_preview_failed_pane_reaps_once_without_restart_loop()->Result<()>{
        let preview=exited_preview(7,true)?;let workspace=preview.workspace.clone();let path=preview.request.path.clone();
        let mut quick=None;let mut pane=Some(preview);
        let errors=poll_previews(&mut quick,&mut pane);
        assert_eq!(errors.len(),1,"the worker failure must remain visible");
        assert!(errors[0].contains("预览宿主异常退出"));
        let completed=pane.as_mut().expect("pane retry barrier must survive worker exit");
        assert!(completed.child.borrow().is_none()&&completed.job.is_none());
        assert!(!workspace.exists());assert!(!completed.ready.get());assert!(completed.hwnd.get().0.is_null());
        assert!(!completed.replace_pane(PreviewProviderPreference::System,&path),"same selection would restart the failed worker");
        assert!(completed.replace_pane(PreviewProviderPreference::System,&path.with_extension("pdf")));
        assert!(completed.replace_pane(PreviewProviderPreference::Wps,&path));
        completed.set_dark(true)?;
        assert!(!workspace.exists(),"a theme update recreated completed protocol files");
        assert!(!needs_timer(&quick,&pane));
        assert!(poll_previews(&mut quick,&mut pane).is_empty(),"worker error was repeated");
        Ok(())
    }
}
