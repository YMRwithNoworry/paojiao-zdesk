//! Optional Everything folder sizes in the native details view's Size column.
//! SetViewProperty is a view cache only: no property store is committed and no
//! filesystem metadata is written. Search/library views do not support it.

use std::{collections::HashMap, path::{Path,PathBuf}, sync::{Arc,Condvar,Mutex,
    atomic::{AtomicBool,AtomicU64,Ordering}}, thread::{self,JoinHandle}, time::{Duration,Instant}};
use windows::{core::{Interface,HRESULT},Win32::{
    Graphics::Gdi::InvalidateRect,
    System::{Com::{CoTaskMemFree,StructuredStorage::{PROPVARIANT,PropVariantClear,PropVariantToUInt64}},
        SystemServices::{SFGAO_FILESYSTEM,SFGAO_FOLDER,SFGAO_STREAM,SFGAO_LINK},Variant::VT_UI8},
    UI::{Shell::{IFolderView2,IShellView,IShellItem,IColumnManager,CM_ENUM_VISIBLE,FVM_DETAILS,
        FOLDERVIEWMODE,ILGetSize,Common::ITEMIDLIST,SIGDN_FILESYSPATH}},
}};
use zsdesk_core::model::explorer::ShellLocation;
use zsdesk_windows::shell::{ExplorerBrowserHost,commands::PKEY_SIZE};

const MAX_VISIBLE:usize=128;
const MAX_REQUEST:usize=64;
const MAX_CACHE:usize=256;
const POLL_INTERVAL:Duration=Duration::from_millis(600);
const CACHE_AGE:Duration=Duration::from_secs(30);
const RETRY_DELAY:Duration=Duration::from_secs(15);

#[derive(Clone,Debug,Default,PartialEq,Eq)]
pub(super) enum Status {
    #[default] Disabled,
    WaitingForDetails,
    Pending,
    Ready{known:usize,unknown:usize},
    Unavailable(String),
}

struct Variant(PROPVARIANT);
impl Variant {
    #[cfg(test)] fn empty()->Self{Self(PROPVARIANT::default())}
    fn size(value:u64)->Self{
        let mut result=PROPVARIANT::default();
        unsafe {(*result.Anonymous.Anonymous).vt=VT_UI8;(*result.Anonymous.Anonymous).Anonymous.uhVal=value;}
        Self(result)
    }
}
impl Drop for Variant{fn drop(&mut self){let _=unsafe {PropVariantClear(&mut self.0)};}}

#[derive(Clone)]
struct ChildPidl(Vec<u16>);
impl ChildPidl{
    fn from_view(view:&IFolderView2,index:i32)->Option<Self>{
        let pointer=unsafe {view.Item(index)}.ok()?;
        if pointer.is_null(){return None;}
        let length=unsafe {ILGetSize(Some(pointer))} as usize;
        let result=if (2..=8192).contains(&length){
            let mut storage=vec![0u16;length.div_ceil(2)];
            unsafe {std::ptr::copy_nonoverlapping(pointer.cast::<u8>(),storage.as_mut_ptr().cast::<u8>(),length);}
            Some(Self(storage))
        }else{None};
        unsafe {CoTaskMemFree(Some(pointer.cast()))};result
    }
    fn pointer(&self)->*const ITEMIDLIST{self.0.as_ptr().cast()}
}
struct VisibleFolder{path:PathBuf,key:String,pidl:ChildPidl}
struct Cached{size:Option<u64>,at:Instant}
struct Applied{pidl:ChildPidl,original:Variant,size:u64,at:Instant}

struct Request{generation:u64,profile:PathBuf,paths:Vec<PathBuf>}
struct Reply{generation:u64,result:Result<Vec<(PathBuf,Option<u64>)>,String>}
#[derive(Default)]
struct Mailbox{request:Option<Request>,reply:Option<Reply>}
struct Shared{mailbox:Mutex<Mailbox>,wake:Condvar,generation:AtomicU64,stop:AtomicBool}
struct Worker{shared:Arc<Shared>,thread:Option<JoinHandle<()>>}
impl Worker{
    fn new()->std::io::Result<Self>{
        let shared=Arc::new(Shared{mailbox:Mutex::new(Mailbox::default()),wake:Condvar::new(),
            generation:AtomicU64::new(0),stop:AtomicBool::new(false)});
        let state=shared.clone();
        let thread=thread::Builder::new().name("zsdesk-folder-sizes".into()).spawn(move||{
            loop{
                let request={
                    let mut mailbox=state.mailbox.lock().unwrap_or_else(|e|e.into_inner());
                    while mailbox.request.is_none()&&!state.stop.load(Ordering::Acquire){
                        mailbox=state.wake.wait(mailbox).unwrap_or_else(|e|e.into_inner());
                    }
                    if state.stop.load(Ordering::Acquire){break;}
                    mailbox.request.take().unwrap()
                };
                let cancelled=||state.stop.load(Ordering::Acquire)||state.generation.load(Ordering::Acquire)!=request.generation;
                let result=crate::fast_search::everything::query_folder_sizes(&request.profile,&request.paths,
                    request.generation as u32,&cancelled).map_err(|error|format!("{error:#}"));
                if cancelled(){continue;}
                let mut mailbox=state.mailbox.lock().unwrap_or_else(|e|e.into_inner());
                if !cancelled(){mailbox.reply=Some(Reply{generation:request.generation,result});}
            }
        })?;
        Ok(Self{shared,thread:Some(thread)})
    }
    fn request(&self,request:Request){
        self.shared.generation.store(request.generation,Ordering::Release);
        let mut mailbox=self.shared.mailbox.lock().unwrap_or_else(|e|e.into_inner());
        mailbox.reply=None;mailbox.request=Some(request);self.shared.wake.notify_one();
    }
    fn cancel(&self,generation:u64){
        self.shared.generation.store(generation,Ordering::Release);
        let mut mailbox=self.shared.mailbox.lock().unwrap_or_else(|e|e.into_inner());
        mailbox.request=None;mailbox.reply=None;
    }
    fn take(&self)->Option<Reply>{self.shared.mailbox.lock().unwrap_or_else(|e|e.into_inner()).reply.take()}
}
impl Drop for Worker{
    fn drop(&mut self){
        self.shared.stop.store(true,Ordering::Release);self.shared.wake.notify_one();
        // Never wait on IPC or engine readiness on the window thread.
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished){if let Some(thread)=self.thread.take(){let _=thread.join();}}
    }
}

/// One controller per displayed Shell pane; never share it across two views.
#[derive(Default)]
pub(super) struct FolderSizes{
    worker:Option<Worker>,generation:u64,pending:bool,
    view:Option<IShellView>,location:Option<PathBuf>,profile:Option<PathBuf>,
    cache:HashMap<String,Cached>,applied:HashMap<String,Applied>,
    last_poll:Option<Instant>,retry_after:Option<Instant>,blocked:bool,status:Status,
}
impl FolderSizes{
    pub(super) fn status(&self)->&Status{&self.status}

    /// Call before navigation/replacing a view, when disabling, and before COM
    /// teardown. The bounded restoration only touches the old view's cache.
    pub(super) fn cancel(&mut self){
        self.generation=self.generation.wrapping_add(1);
        if let Some(worker)=&self.worker{worker.cancel(self.generation);}
        self.pending=false;
        if let Some(view)=&self.view{
            if let Ok(folder)=view.cast::<IFolderView2>(){
                for applied in self.applied.values(){let _=unsafe {folder.SetViewProperty(applied.pidl.pointer(),&PKEY_SIZE,&applied.original.0)};}
                invalidate(view);
            }
        }
        self.applied.clear();self.cache.clear();self.view=None;self.location=None;self.profile=None;
        self.last_poll=None;self.retry_after=None;self.blocked=false;self.status=Status::Disabled;
    }

    /// Poll from the owning STA while its COM apartment and Shell view live.
    /// Returns a new unavailable explanation once; it does not show a dialog.
    pub(super) fn poll(&mut self,browser:Option<&ExplorerBrowserHost>,location:&ShellLocation,
        enabled:bool,navigating:bool,profile:&Path)->Option<String>{
        if !enabled||navigating{
            if self.view.is_some()||self.pending||self.status!=Status::Disabled{self.cancel();}
            if !enabled{self.worker=None;}
            return None;
        }
        let ShellLocation::Path(path)=location else{
            if self.view.is_some()||self.pending{self.cancel();}
            return self.unavailable("文件夹大小仅支持普通文件夹的详细信息视图；搜索和库视图不支持该显示接口".into());
        };
        let Some(view)=browser.and_then(|browser|unsafe {browser.current_view()}.ok())else{return None;};
        let same=self.view.as_ref().is_some_and(|old|old.as_raw()==view.as_raw())
            &&self.location.as_ref()==Some(path)&&self.profile.as_deref()==Some(profile);
        if !same{
            self.cancel();self.view=Some(view.clone());self.location=Some(path.clone());self.profile=Some(profile.into());
        }
        if self.blocked{return None;}
        if self.last_poll.is_some_and(|last|last.elapsed()<POLL_INTERVAL){return None;}
        self.last_poll=Some(Instant::now());
        let folder=match view.cast::<IFolderView2>(){Ok(folder)=>folder,Err(_)=>{self.blocked=true;return self.unavailable("此文件视图不支持文件夹大小显示".into());}};
        if !details_with_size_column(&view,&folder){
            if self.pending{
                self.generation=self.generation.wrapping_add(1);
                if let Some(worker)=&self.worker{worker.cancel(self.generation);}
                self.pending=false;
            }
            self.status=Status::WaitingForDetails;return None;
        }
        if let Some(reply)=self.worker.as_ref().and_then(Worker::take){
            if reply.generation==self.generation{
                self.pending=false;
                match reply.result{
                    Ok(values)=>for (path,size) in values{
                        if self.cache.len()>=MAX_CACHE{evict_cache(&mut self.cache);}
                        self.cache.insert(path_key(&path),Cached{size,at:Instant::now()});
                    },
                    Err(error)=>{
                        self.retry_after=Some(Instant::now()+RETRY_DELAY);
                        return self.unavailable(format!("文件夹大小不可用：{error}"));
                    }
                }
            }
        }
        let visible=match visible_folders(&folder){Ok(visible)=>visible,Err(error)=>{
            self.retry_after=Some(Instant::now()+RETRY_DELAY);
            return self.unavailable(format!("无法读取可见文件夹：{error}"));
        }};
        let mut request=Vec::new();let mut known=0;let mut unknown=0;let mut changed=false;
        for entry in visible{
            let cached=self.cache.get(&entry.key).filter(|cached|cached.at.elapsed()<CACHE_AGE).map(|cached|cached.size);
            if let Some(Some(size))=cached{
                known+=1;
                if self.applied.get(&entry.key).is_some_and(|applied|applied.size==size){continue;}
                if self.applied.len()>=MAX_CACHE{self.evict_applied(&folder);}
                let original=if let Some(applied)=self.applied.remove(&entry.key){applied.original}
                    else{Variant(unsafe {folder.GetViewProperty(entry.pidl.pointer(),&PKEY_SIZE)}.unwrap_or_default())};
                if let Err(error)=write_size(&folder,&entry.pidl,size){
                    let _=unsafe {folder.SetViewProperty(entry.pidl.pointer(),&PKEY_SIZE,&original.0)};
                    self.blocked=true;invalidate(&view);
                    return self.unavailable(format!("系统文件视图未接受文件夹大小缓存：{error}"));
                }
                self.applied.insert(entry.key,Applied{pidl:entry.pidl,original,size,at:Instant::now()});changed=true;
            }else{
                // Unknown and expired values stay native/blank, never zero.
                if let Some(applied)=self.applied.remove(&entry.key){let _=unsafe {folder.SetViewProperty(applied.pidl.pointer(),&PKEY_SIZE,&applied.original.0)};changed=true;}
                if cached.is_some(){unknown+=1;}
                else if request.len()<MAX_REQUEST{request.push(entry.path);}
            }
        }
        if changed{invalidate(&view);}
        if !request.is_empty()&&!self.pending&&self.retry_after.is_none_or(|until|Instant::now()>=until){
            if self.worker.is_none(){match Worker::new(){Ok(worker)=>self.worker=Some(worker),Err(error)=>{
                self.retry_after=Some(Instant::now()+RETRY_DELAY);return self.unavailable(format!("无法启动文件夹大小查询：{error}"));
            }}}
            self.generation=self.generation.wrapping_add(1);self.pending=true;self.retry_after=None;
            self.worker.as_ref().unwrap().request(Request{generation:self.generation,profile:profile.into(),paths:request});
        }
        if self.retry_after.is_none_or(|until|Instant::now()>=until){
            self.status=if self.pending{Status::Pending}else{Status::Ready{known,unknown}};
        }
        None
    }
    fn unavailable(&mut self,message:String)->Option<String>{
        let status=Status::Unavailable(message.clone());
        if self.status==status{return None;}
        self.status=status;Some(message)
    }
    fn evict_applied(&mut self,folder:&IFolderView2){
        let oldest=self.applied.iter().min_by_key(|(_,entry)|entry.at).map(|(key,_)|key.clone());
        if let Some(applied)=oldest.and_then(|key|self.applied.remove(&key)){
            let _=unsafe {folder.SetViewProperty(applied.pidl.pointer(),&PKEY_SIZE,&applied.original.0)};
        }
    }
}
impl Drop for FolderSizes{fn drop(&mut self){self.cancel();}}

fn path_key(path:&Path)->String{path.to_string_lossy().to_lowercase()}
fn evict_cache(cache:&mut HashMap<String,Cached>){
    let oldest=cache.iter().min_by_key(|(_,entry)|entry.at).map(|(key,_)|key.clone());
    if let Some(key)=oldest{cache.remove(&key);}
}
fn invalidate(view:&IShellView){if let Ok(hwnd)=unsafe {view.GetWindow()}{let _=unsafe {InvalidateRect(Some(hwnd),None,false)};}}
fn details_with_size_column(view:&IShellView,folder:&IFolderView2)->bool{
    let mut mode=FOLDERVIEWMODE(0);let mut size=0;
    if unsafe {folder.GetViewModeAndIconSize(&mut mode,&mut size)}.is_err()||mode!=FVM_DETAILS{return false;}
    let Ok(columns)=view.cast::<IColumnManager>()else{return false;};
    let Ok(count)=(unsafe {columns.GetColumnCount(CM_ENUM_VISIBLE)})else{return false;};
    if count==0||count>128{return false;}
    let mut keys=vec![PKEY_SIZE;count as usize];
    unsafe {columns.GetColumns(CM_ENUM_VISIBLE,&mut keys)}.is_ok()&&keys.contains(&PKEY_SIZE)
}
fn visible_folders(view:&IFolderView2)->windows::core::Result<Vec<VisibleFolder>>{
    let mut folders=Vec::new();let mut last=-1;
    for _ in 0..MAX_VISIBLE{
        let mut index=-1;
        let result=unsafe {(view.vtable().GetVisibleItem)(view.as_raw(),last,false.into(),&mut index)};
        if result==HRESULT(1){break;}
        result.ok()?;if index<=last{break;}last=index;
        let item:IShellItem=unsafe {view.GetItem(index)?};
        let attributes=unsafe {item.GetAttributes(SFGAO_FILESYSTEM|SFGAO_FOLDER|SFGAO_STREAM|SFGAO_LINK)?}.0;
        if attributes&(SFGAO_FILESYSTEM.0|SFGAO_FOLDER.0)!=(SFGAO_FILESYSTEM.0|SFGAO_FOLDER.0)
            ||attributes&(SFGAO_STREAM.0|SFGAO_LINK.0)!=0{continue;}
        let value=unsafe {item.GetDisplayName(SIGDN_FILESYSPATH)?};
        let path=unsafe {value.to_string()}.map(PathBuf::from);unsafe {CoTaskMemFree(Some(value.0.cast()))};
        let path=path?;if !path.is_absolute(){continue;}
        let Some(pidl)=ChildPidl::from_view(view,index)else{continue;};
        folders.push(VisibleFolder{key:path_key(&path),path,pidl});
    }
    Ok(folders)
}
fn write_size(view:&IFolderView2,pidl:&ChildPidl,size:u64)->windows::core::Result<()>{
    let value=Variant::size(size);
    unsafe {view.SetViewProperty(pidl.pointer(),&PKEY_SIZE,&value.0)?;}
    let readback=Variant(unsafe {view.GetViewProperty(pidl.pointer(),&PKEY_SIZE)?});
    if unsafe {PropVariantToUInt64(&readback.0)}?!=size{
        return Err(windows::core::Error::from_hresult(HRESULT(0x80004005u32 as i32)));
    }
    Ok(())
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn folder_size_unknown_is_distinct_from_empty_and_large_values(){
        assert_ne!(None,Some(0u64));
        for size in [0,1,u32::MAX as u64+123,u64::MAX-1]{
            let value=Variant::size(size);assert_eq!(unsafe {PropVariantToUInt64(&value.0)}.unwrap(),size);
        }
        let empty=Variant::empty();assert_ne!(unsafe {empty.0.Anonymous.Anonymous.vt},VT_UI8);
    }
    #[test]
    fn disabled_folder_sizes_have_no_worker_or_query(){
        let mut controller=FolderSizes::default();
        assert!(controller.poll(None,&ShellLocation::ThisPc,false,false,Path::new(r"C:\profile.json")).is_none());
        assert!(controller.worker.is_none());assert!(controller.view.is_none());assert!(!controller.pending);
        assert_eq!(controller.status(),&Status::Disabled);
    }
    #[test]
    fn cancellation_discards_queued_and_completed_size_replies(){
        let shared=Arc::new(Shared{mailbox:Mutex::new(Mailbox{request:Some(Request{generation:1,profile:PathBuf::new(),paths:vec![]}),
            reply:Some(Reply{generation:1,result:Ok(vec![])})}),wake:Condvar::new(),generation:AtomicU64::new(1),stop:AtomicBool::new(false)});
        let worker=Worker{shared,thread:None};worker.cancel(2);
        assert_eq!(worker.shared.generation.load(Ordering::Acquire),2);assert!(worker.take().is_none());
        assert!(worker.shared.mailbox.lock().unwrap().request.is_none());
    }

    #[test]
    #[ignore="Native Shell Size-column cache regression; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_folder_size_column_is_view_only_and_reversible()->anyhow::Result<()>{
        use anyhow::{Context,ensure};
        use windows::{core::PCWSTR,Win32::{Foundation::{HINSTANCE,HWND,LPARAM,LRESULT,RECT,WPARAM},
            System::{LibraryLoader::GetModuleHandleW,Ole::{OleInitialize,OleUninitialize}},
            UI::{Controls::{InitCommonControlsEx,INITCOMMONCONTROLSEX,ICC_LISTVIEW_CLASSES},
                Shell::{IShellItem2,SVGIO_ALLVIEW},WindowsAndMessaging::*}}};
        use zsdesk_windows::shell::{Panes,WM_SHELL_NAVIGATED,WM_SHELL_FOLDER_INVOKED,
            WM_SHELL_SELECTION_CHANGED,acknowledge_selection_changed,shell_item};
        fn pump(){
            let until=Instant::now()+Duration::from_millis(40);
            while Instant::now()<until{
                let mut message=MSG::default();
                for _ in 0..128{
                    if !unsafe {PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
                    unsafe {let _=TranslateMessage(&message);DispatchMessageW(&message);}
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
        unsafe extern "system" fn owner(window:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
            if matches!(message,WM_SHELL_NAVIGATED|WM_SHELL_FOLDER_INVOKED){
                if lp.0!=0{unsafe {drop(Box::from_raw(lp.0 as*mut ShellLocation));}}return LRESULT(0);
            }
            if message==WM_SHELL_SELECTION_CHANGED{unsafe {acknowledge_selection_changed(lp.0);}return LRESULT(0);}
            unsafe {DefWindowProcW(window,message,wp,lp)}
        }
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe {OleUninitialize();}}}
        struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe {DestroyWindow(self.0)};}}
        struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=std::fs::remove_file(self.0.join("folder/read-only-marker.txt"));
            let _=std::fs::remove_dir(self.0.join("folder"));let _=std::fs::remove_dir(&self.0);}}
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("native test output")?);
        ensure!(output.is_absolute(),"test output must be absolute");
        let path=output.join("folder-size-view-fixture");ensure!(!path.exists(),"fixture must be new");
        std::fs::create_dir_all(&path)?;let fixture=Fixture(path.clone());
        std::fs::create_dir(path.join("folder"))?;std::fs::write(path.join("folder/read-only-marker.txt"),b"unchanged")?;
        unsafe {OleInitialize(None)?;}let apartment=Apartment;
        unsafe {InitCommonControlsEx(&INITCOMMONCONTROLSEX{dwSize:std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,dwICC:ICC_LISTVIEW_CLASSES})}.ok()?;
        let instance=HINSTANCE(unsafe {GetModuleHandleW(None)?}.0);let class=windows::core::w!("ZSDesk.FolderSize.ColumnRegression");
        unsafe {RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(owner),hInstance:instance,lpszClassName:class,..Default::default()});}
        let foreground=unsafe {GetForegroundWindow()};
        let window=Window(unsafe {CreateWindowExW(WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW,class,PCWSTR::null(),WS_POPUP|WS_CLIPCHILDREN,
            -28000,-28000,800,500,None,None,Some(instance),None)?});
        let browser=unsafe {ExplorerBrowserHost::create_transient(window.0,RECT{left:0,top:0,right:800,bottom:500},false,Panes::default())?};
        unsafe {let _=ShowWindow(window.0,SW_SHOWNOACTIVATE);browser.navigate(&ShellLocation::Path(path.clone()))?;}
        let started=Instant::now();let (shell,folder)=loop{
            pump();
            if let Ok(shell)=unsafe {browser.current_view()}{if let Ok(folder)=shell.cast::<IFolderView2>(){
                if unsafe {folder.ItemCount(SVGIO_ALLVIEW)}.unwrap_or(0)==1{break (shell,folder);}
            }}
            ensure!(started.elapsed()<Duration::from_secs(10),"fixture navigation timeout");
        };
        browser.set_view_mode(FVM_DETAILS.0,16)?;
        let columns:IColumnManager=shell.cast()?;
        unsafe {columns.SetColumns(&[zsdesk_windows::shell::commands::PKEY_NAME,PKEY_SIZE])?;}
        pump();ensure!(details_with_size_column(&shell,&folder),"native Size column is not visible");
        let pidl=ChildPidl::from_view(&folder,0).context("folder pidl")?;
        let item:IShellItem2=unsafe {shell_item(&ShellLocation::Path(path.join("folder")))?}.cast()?;
        let disk_before=unsafe {item.GetUInt64(&PKEY_SIZE)}.ok();
        let original=Variant(unsafe {folder.GetViewProperty(pidl.pointer(),&PKEY_SIZE)}.unwrap_or_default());
        let bytes=u32::MAX as u64+4097;
        write_size(&folder,&pidl,bytes)?;invalidate(&shell);pump();
        let fresh:IShellItem2=unsafe {shell_item(&ShellLocation::Path(path.join("folder")))?}.cast()?;
        ensure!(unsafe {fresh.GetUInt64(&PKEY_SIZE)}.ok()==disk_before,"view update changed the item's actual size property");
        ensure!(std::fs::read(path.join("folder/read-only-marker.txt"))?==b"unchanged","view update changed file contents");
        let mut controller=FolderSizes::default();controller.view=Some(shell.clone());
        controller.applied.insert(path_key(&path.join("folder")),Applied{pidl:pidl.clone(),original,size:bytes,at:Instant::now()});
        controller.cancel();pump();
        let restored=Variant(unsafe {folder.GetViewProperty(pidl.pointer(),&PKEY_SIZE)}.unwrap_or_default());
        ensure!(unsafe {restored.0.Anonymous.Anonymous.vt}!=VT_UI8||unsafe {PropVariantToUInt64(&restored.0)}.ok()!=Some(bytes),"disable left injected folder size behind");
        drop(controller);drop(restored);drop(fresh);drop(item);drop(folder);drop(shell);drop(browser);drop(window);pump();drop(apartment);drop(fixture);
        ensure!(unsafe {GetForegroundWindow()}==foreground,"native regression changed the foreground");
        Ok(())
    }
}
