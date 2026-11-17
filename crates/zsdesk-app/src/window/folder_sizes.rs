//! View-local, cancellable total sizes for ordinary filesystem views.
use std::{collections::{HashSet,VecDeque},ffi::OsString,path::{Path,PathBuf},sync::{Arc,Condvar,Mutex,
    atomic::{AtomicBool,AtomicU64,Ordering}},thread::{self,JoinHandle},time::{Duration,Instant}};
use std::os::windows::ffi::{OsStringExt,OsStrExt};
use windows::{core::{Interface,HRESULT,IUnknown},Win32::{Foundation::HWND,
    Graphics::Gdi::InvalidateRect,System::Com::{CoTaskMemFree,StructuredStorage::{PROPVARIANT,PropVariantClear,PropVariantToUInt64}},
    System::SystemServices::SFGAO_FILESYSTEM,System::Variant::VT_UI8,
    UI::{Shell::{IFolderView2,IShellView,IShellFolder,IPersistFolder2,IEnumIDList,IColumnManager,CM_ENUM_VISIBLE,
        FVM_DETAILS,FOLDERVIEWMODE,ILGetSize,Common::ITEMIDLIST,SIGDN_FILESYSPATH,SVGIO_ALLVIEW,SHCreateItemWithParent,IShellItem},
        WindowsAndMessaging::WM_APP}}};
use zsdesk_core::model::explorer::ShellLocation;
use zsdesk_windows::shell::{ExplorerBrowserHost,folder_totals::{self,Columns,PKEY_TOTAL_SIZE},notify::ChangeNotify};
use crate::folder_size_data::{self,Item,Value};
#[path="folder_size_ranking.rs"]mod ranking;
pub(super) fn show_ranking(owner:HWND,identity:super::pane_input::Identity,profile:&Path,directory:&Path)->anyhow::Result<()>{ranking::show(owner,identity,profile,directory)}
pub(super) const WM_CHANGED:u32=WM_APP+89;
const MAX_ITEMS:usize=32_768;
const MAX_BYTES:usize=16*1024*1024;
const CHUNK:usize=128;
const REFRESH:Duration=Duration::from_secs(30);

#[derive(Clone,Debug,Default,PartialEq,Eq)]
pub(super) enum Status{#[default]Disabled,WaitingForDetails,Reading,Pending{done:usize,total:usize},
    Ready{known:usize,unknown:usize,skipped_links:u64},Unavailable(String)}
struct Variant(PROPVARIANT);
impl Variant{fn size(value:u64)->Self{let mut result=PROPVARIANT::default();unsafe{(*result.Anonymous.Anonymous).vt=VT_UI8;(*result.Anonymous.Anonymous).Anonymous.uhVal=value;}Self(result)}}
impl Drop for Variant{fn drop(&mut self){let _=unsafe{PropVariantClear(&mut self.0)};}}
#[derive(Clone)]struct ChildPidl(Vec<u16>);
impl ChildPidl{
    fn take(pointer:*mut ITEMIDLIST)->windows::core::Result<Self>{
        if pointer.is_null(){return Err(error(zsdesk_core::i18n::text("空项目标识"),0x80070057));}
        let length=unsafe{ILGetSize(Some(pointer))}as usize;
        let result=if (2..=8192).contains(&length){let mut data=vec![0u16;length.div_ceil(2)];
            unsafe{std::ptr::copy_nonoverlapping(pointer.cast::<u8>(),data.as_mut_ptr().cast::<u8>(),length);}Ok(Self(data))}
            else{Err(error(zsdesk_core::i18n::text("项目标识长度无效"),0x80070057))};
        unsafe{CoTaskMemFree(Some(pointer.cast()))};result
    }
    fn pointer(&self)->*const ITEMIDLIST{self.0.as_ptr().cast()}
}
fn error(message:&str,code:u32)->windows::core::Error{windows::core::Error::new(HRESULT(code as i32),message)}
fn stage(name:&str,error:windows::core::Error)->windows::core::Error{windows::core::Error::new(error.code(),format!("{name}（0x{:08X}）：{}",error.code().0 as u32,error.message()))}
#[derive(Clone)]struct Row{item:Item,pidl:ChildPidl}
struct Gather{enumerator:IEnumIDList,folder:IShellFolder,rows:Vec<Row>,names:HashSet<Vec<u16>>,bytes:usize}
impl Gather{
    fn new(view:&IFolderView2)->windows::core::Result<Self>{Ok(Self{enumerator:unsafe{view.Items(SVGIO_ALLVIEW)}?,folder:unsafe{view.GetFolder()}?,rows:Vec::new(),names:HashSet::new(),bytes:0})}
    fn step(&mut self)->windows::core::Result<bool>{
        for _ in 0..CHUNK{
            let mut pointers=[std::ptr::null_mut()];let mut fetched=0;
            let status=unsafe{self.enumerator.Next(&mut pointers,Some(&mut fetched))};
            status.ok().map_err(|e|stage(zsdesk_core::i18n::text("枚举文件夹项目"),e))?;
            if fetched==0{return Ok(true);}
            let pidl=ChildPidl::take(pointers[0])?;
            let item:IShellItem=unsafe{SHCreateItemWithParent(None,&self.folder,pidl.pointer())}.map_err(|e|stage(zsdesk_core::i18n::text("读取文件夹项目"),e))?;
            if unsafe{item.GetAttributes(SFGAO_FILESYSTEM)}?.0&SFGAO_FILESYSTEM.0==0{continue;}
            let value=unsafe{item.GetDisplayName(SIGDN_FILESYSPATH)}.map_err(|e|stage(zsdesk_core::i18n::text("读取项目路径"),e))?;
            if value.is_null(){continue;}
            let path=unsafe{PathBuf::from(OsString::from_wide(value.as_wide()))};unsafe{CoTaskMemFree(Some(value.0.cast()))};
            let Some(name)=path.file_name().map(|name|name.to_os_string())else{continue;};
            if !self.names.insert(pidl.0.clone()){return Err(error(zsdesk_core::i18n::text("目录正在变化，正在重新读取"),0x8000000a));}
            self.bytes=self.bytes.saturating_add(pidl.0.len()*2+name.as_encoded_bytes().len()+64);
            if self.rows.len()>=MAX_ITEMS||self.bytes>MAX_BYTES{return Err(error(zsdesk_core::i18n::text("目录项目过多，无法完整计算总大小；已保留原生大小列"),0x8007000e));}
            self.rows.push(Row{item:Item{name},pidl});
            if status==HRESULT(1){return Ok(true);}
        }Ok(false)
    }
}
// Kernel change handles are thread independent. Creation may contact a remote
// filesystem, so it runs on the calculation worker; UI polling never waits.
struct FilesystemWatch(usize);
impl FilesystemWatch{
    fn new(directory:&Path)->anyhow::Result<Self>{
        use windows::Win32::Storage::FileSystem::{FindFirstChangeNotificationW,FILE_NOTIFY_CHANGE_FILE_NAME,
            FILE_NOTIFY_CHANGE_DIR_NAME,FILE_NOTIFY_CHANGE_SIZE,FILE_NOTIFY_CHANGE_LAST_WRITE,FILE_NOTIFY_CHANGE_ATTRIBUTES};
        let directory=std::fs::canonicalize(directory)?;let name:Vec<u16>=directory.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle=unsafe{FindFirstChangeNotificationW(windows::core::PCWSTR(name.as_ptr()),true,
            FILE_NOTIFY_CHANGE_FILE_NAME|FILE_NOTIFY_CHANGE_DIR_NAME|FILE_NOTIFY_CHANGE_SIZE|FILE_NOTIFY_CHANGE_LAST_WRITE|FILE_NOTIFY_CHANGE_ATTRIBUTES)}?;
        Ok(Self(handle.0 as usize))
    }
    fn changed(&self)->windows::core::Result<bool>{
        use windows::Win32::{Foundation::{HANDLE,WAIT_OBJECT_0,WAIT_TIMEOUT},System::Threading::WaitForSingleObject,
            Storage::FileSystem::FindNextChangeNotification};
        let handle=HANDLE(self.0 as*mut _);
        match unsafe{WaitForSingleObject(handle,0)}{
            WAIT_TIMEOUT=>Ok(false),WAIT_OBJECT_0=>{unsafe{FindNextChangeNotification(handle)}?;Ok(true)},
            _=>Err(windows::core::Error::from_thread()),
        }
    }
}
impl Drop for FilesystemWatch{fn drop(&mut self){let _=unsafe{windows::Win32::Storage::FileSystem::FindCloseChangeNotification(windows::Win32::Foundation::HANDLE(self.0 as*mut _))};}}
struct Request{generation:u64,profile:PathBuf,directory:PathBuf,rows:Arc<Vec<Row>>,watch_needed:bool}
enum ResultValue{Value(usize,Value),Done(Option<String>),Watch(Option<FilesystemWatch>)}
struct Reply{generation:u64,value:ResultValue}
#[derive(Default)]struct Mailbox{request:Option<Request>,replies:VecDeque<Reply>}
struct Shared{mailbox:Mutex<Mailbox>,wake:Condvar,generation:AtomicU64,stop:AtomicBool}
struct Worker{shared:Arc<Shared>,thread:Option<JoinHandle<()>>}
impl Worker{
    fn new()->std::io::Result<Self>{
        let shared=Arc::new(Shared{mailbox:Mutex::new(Mailbox::default()),wake:Condvar::new(),generation:AtomicU64::new(0),stop:AtomicBool::new(false)});
        let state=shared.clone();let thread=thread::Builder::new().name("zsdesk-folder-sizes".into()).spawn(move||{
            use windows::Win32::System::Threading::{GetCurrentThread,SetThreadPriority,THREAD_MODE_BACKGROUND_BEGIN};
            let _=unsafe{SetThreadPriority(GetCurrentThread(),THREAD_MODE_BACKGROUND_BEGIN)};
            loop{
                let request={let mut mailbox=state.mailbox.lock().unwrap_or_else(|e|e.into_inner());
                    while mailbox.request.is_none()&&!state.stop.load(Ordering::Acquire){mailbox=state.wake.wait(mailbox).unwrap_or_else(|e|e.into_inner());}
                    if state.stop.load(Ordering::Acquire){break;}mailbox.request.take().unwrap()};
                let cancelled=||state.stop.load(Ordering::Acquire)||state.generation.load(Ordering::Acquire)!=request.generation;
                let send=|value:ResultValue|{let mut mailbox=state.mailbox.lock().unwrap_or_else(|e|e.into_inner());
                    while mailbox.replies.len()>=64&&!cancelled(){mailbox=state.wake.wait_timeout(mailbox,Duration::from_millis(20)).unwrap_or_else(|e|e.into_inner()).0;}
                    if cancelled(){return false;}mailbox.replies.push_back(Reply{generation:request.generation,value});true};
                if request.watch_needed&&!cancelled(){send(ResultValue::Watch(FilesystemWatch::new(&request.directory).ok()));}
                let items:Vec<_>=request.rows.iter().map(|row|row.item.clone()).collect();
                let result=folder_size_data::calculate(&request.profile,&request.directory,&items,&cancelled,|index,value|send(ResultValue::Value(index,value)));
                if !cancelled(){send(ResultValue::Done(result.err().map(|error|format!("{error:#}"))));}
            }
        })?;Ok(Self{shared,thread:Some(thread)})
    }
    fn request(&self,request:Request){self.shared.generation.store(request.generation,Ordering::Release);
        let mut mailbox=self.shared.mailbox.lock().unwrap_or_else(|e|e.into_inner());mailbox.replies.clear();mailbox.request=Some(request);self.shared.wake.notify_all();}
    fn cancel(&self,generation:u64){self.shared.generation.store(generation,Ordering::Release);let mut mailbox=self.shared.mailbox.lock().unwrap_or_else(|e|e.into_inner());
        mailbox.request=None;mailbox.replies.clear();self.shared.wake.notify_all();}
    fn take(&self)->Vec<Reply>{let mut mailbox=self.shared.mailbox.lock().unwrap_or_else(|e|e.into_inner());let result=mailbox.replies.drain(..).collect();self.shared.wake.notify_all();result}
}
impl Drop for Worker{fn drop(&mut self){self.shared.stop.store(true,Ordering::Release);self.shared.wake.notify_all();
    if self.thread.as_ref().is_some_and(JoinHandle::is_finished){if let Some(thread)=self.thread.take(){let _=thread.join();}}}}

#[derive(Default)]pub(super) struct FolderSizes{
    worker:Option<Worker>,generation:u64,pending:bool,view:Option<IShellView>,location:Option<PathBuf>,profile:Option<PathBuf>,
    columns:Option<Columns>,notify:Option<ChangeNotify>,watch:Option<FilesystemWatch>,gather:Option<Gather>,rows:Arc<Vec<Row>>,
    originals:Vec<Option<Variant>>,values:Vec<Option<Value>>,count:Option<i32>,dirty:Option<Instant>,
    last_refresh:Option<Instant>,last_poll:Option<Instant>,last_order:Option<Instant>,order_dirty:bool,retry_after:Option<Instant>,failures:u8,blocked:bool,status:Status,
}
impl FolderSizes{
    pub(super) fn status(&self)->&Status{&self.status}
    pub(super) fn active(&self)->bool{self.columns.is_some()}
    pub(super) fn busy(&self)->bool{!self.blocked&&(self.pending||self.gather.is_some()||self.dirty.is_some())}
    pub(super) fn progress(&self)->Option<String>{match &self.status{
        Status::Reading=>Some(zsdesk_core::i18n::text("正在读取大小计算范围…").into()),
        Status::Pending{done,total}=>Some(zsdesk_core::tr_format!("正在计算大小 {done} / {total}", done=done, total=total)),
        Status::Ready{unknown,skipped_links,..} if *unknown>0||*skipped_links>0=>Some(zsdesk_core::tr_format!("大小已更新 · {unknown} 项未知{}", if *skipped_links>0{zsdesk_core::tr_format!(" · 未跟随 {skipped_links} 个目录链接", skipped_links=skipped_links)}else{String::new()}, unknown=unknown)),
        _=>None,
    }}
    pub(super) fn invalidate(&mut self){if self.view.is_some(){self.dirty=Some(Instant::now());self.blocked=false;self.retry_after=None;}}
    fn stop_work(&mut self){
        self.generation=self.generation.wrapping_add(1);if let Some(worker)=&self.worker{worker.cancel(self.generation);}
        self.pending=false;self.gather=None;self.order_dirty=false;self.last_order=None;
        if let Some(folder)=self.view.as_ref().and_then(|view|view.cast::<IFolderView2>().ok()){
            for (row,original) in self.rows.iter().zip(&mut self.originals){if let Some(original)=original.take(){let _=unsafe{folder.SetViewProperty(row.pidl.pointer(),&PKEY_TOTAL_SIZE,&original.0)};}}
        }
        self.rows=Arc::new(Vec::new());self.originals.clear();self.values.clear();
    }
    pub(super) fn cancel(&mut self){
        self.stop_work();self.notify=None;self.watch=None;self.columns=None;self.view=None;self.location=None;self.profile=None;self.count=None;
        self.dirty=None;self.last_refresh=None;self.last_poll=None;self.retry_after=None;self.failures=0;self.blocked=false;self.status=Status::Disabled;
    }
    fn unavailable(&mut self,message:String)->Option<String>{let status=Status::Unavailable(message.clone());if self.status==status{return None;}self.status=status;Some(message)}
    pub(super) fn poll(&mut self,browser:Option<&ExplorerBrowserHost>,location:&ShellLocation,enabled:bool,navigating:bool,profile:&Path)->Option<String>{
        if !enabled||navigating{if self.view.is_some()||self.pending{self.cancel();}if !enabled{self.worker=None;}return None;}
        let ShellLocation::Path(path)=location else{if self.view.is_some(){self.cancel();}return None;};
        let Some(view)=browser.and_then(|browser|unsafe{browser.current_view()}.ok())else{return None;};
        if interaction_active(&view){return None;}
        let same=self.view.as_ref().is_some_and(|old|old.cast::<IUnknown>().ok()==view.cast::<IUnknown>().ok())&&self.location.as_ref()==Some(path)&&self.profile.as_deref()==Some(profile);
        if !same{self.cancel();self.view=Some(view.clone());self.location=Some(path.clone());self.profile=Some(profile.into());}
        if self.last_poll.is_some_and(|last|last.elapsed()<Duration::from_millis(60)){return None;}self.last_poll=Some(Instant::now());
        match self.watch.as_ref().map(FilesystemWatch::changed){
            Some(Ok(true))=>self.invalidate(),Some(Err(_))=>{self.watch=None;self.invalidate();},_=>(),
        }
        let folder=match view.cast::<IFolderView2>(){Ok(folder)=>folder,Err(error)=>return self.unavailable(zsdesk_core::tr_format!("此视图不支持总大小：{error}", error=error))};
        let count=match unsafe{folder.ItemCount(SVGIO_ALLVIEW)}{Ok(count)=>count.max(0),Err(error)=>return self.unavailable(zsdesk_core::tr_format!("无法读取目录项目：{error}", error=error))};
        if self.count.is_some_and(|old|old!=count){self.invalidate();self.count=Some(count);}
        if count as usize>MAX_ITEMS{self.stop_work();self.columns=None;self.watch=None;self.notify=None;self.blocked=true;self.count=Some(count);return self.unavailable(zsdesk_core::i18n::text("目录项目过多，无法完整计算总大小；已保留原生大小列").into());}
        if self.blocked{return None;}
        if self.retry_after.is_some_and(|until|Instant::now()<until){return None;}
        if self.columns.is_none(){match Columns::activate(&view){Ok(columns)=>self.columns=Some(columns),Err(error)=>return self.unavailable(zsdesk_core::tr_format!("此视图不支持总大小列：{error}", error=error))}}
        if let Some(columns)=&mut self.columns{if let Err(error)=columns.synchronize(){return self.unavailable(zsdesk_core::tr_format!("无法更新总大小列：{error}", error=error));}}
        if !details_with_total_column(&view,&folder)&&!folder_totals::ordered_by_totals(&view){
            if self.pending||self.gather.is_some()||!self.rows.is_empty(){self.stop_work();}
            self.last_refresh=None;self.watch=None;self.notify=None;self.dirty=None;
            self.status=Status::WaitingForDetails;return None;
        }
        if self.notify.is_none(){self.notify=register_changes(&view,&folder);}
        if self.dirty.is_some_and(|changed|changed.elapsed()<Duration::from_millis(250)){return None;}
        let refresh=self.dirty.is_some()||self.last_refresh.is_none_or(|last|last.elapsed()>=REFRESH)&&!self.pending&&self.gather.is_none();
        if refresh{
            self.stop_work();self.dirty=None;self.count=Some(count);self.status=Status::Reading;
            if count==0{self.last_refresh=Some(Instant::now());self.status=Status::Ready{known:0,unknown:0,skipped_links:0};return None;}
            match Gather::new(&folder){Ok(gather)=>self.gather=Some(gather),Err(error)=>return self.retry(zsdesk_core::tr_format!("无法枚举目录：{error}", error=error))}
        }
        if let Some(gather)=&mut self.gather{
            match gather.step(){
                Ok(false)=>return None,
                Ok(true)=>{
                    let rows=self.gather.take().unwrap().rows;self.failures=0;self.rows=Arc::new(rows);
                    self.originals=(0..self.rows.len()).map(|_|None).collect();self.values=vec![None;self.rows.len()];
                    if self.rows.is_empty(){self.columns=None;self.notify=None;self.watch=None;self.blocked=true;self.last_refresh=Some(Instant::now());self.status=Status::WaitingForDetails;return None;}
                    if self.worker.is_none(){match Worker::new(){Ok(worker)=>self.worker=Some(worker),Err(error)=>return self.retry(zsdesk_core::tr_format!("无法启动大小计算：{error}", error=error))}}
                    self.generation=self.generation.wrapping_add(1);self.pending=true;self.status=Status::Pending{done:0,total:self.rows.len()};
                    self.worker.as_ref().unwrap().request(Request{generation:self.generation,profile:profile.into(),directory:path.clone(),rows:self.rows.clone(),watch_needed:self.watch.is_none()});
                },
                Err(error)=>{self.gather=None;if error.code().0 as u32==0x8007000e{self.stop_work();self.columns=None;self.watch=None;self.notify=None;self.blocked=true;return self.unavailable(error.message());}return self.retry(zsdesk_core::tr_format!("读取目录项目失败：{error}", error=error));}
            }
        }
        let replies=self.worker.as_ref().map(Worker::take).unwrap_or_default();let mut changed=false;
        for reply in replies{
            if reply.generation!=self.generation{continue;}
            match reply.value{
                ResultValue::Watch(watch)=>self.watch=watch,
                ResultValue::Value(index,mut value)=>{
                    let Some(row)=self.rows.get(index)else{continue;};
                    if let Some(size)=value.bytes{
                        if self.originals[index].is_none(){self.originals[index]=Some(Variant(unsafe{folder.GetViewProperty(row.pidl.pointer(),&PKEY_TOTAL_SIZE)}.unwrap_or_default()));}
                        if let Err(error)=write_size(&folder,&row.pidl,size){super::diagnostics::record("folder-sizes",&zsdesk_core::tr_format!("设置总大小：{error}", error=error),Some(error.code().0));self.invalidate();continue;}
                        changed=true;
                    }
                    if let Some(error)=value.error.take(){super::diagnostics::record("folder-sizes",&error,None);}
                    self.values[index]=Some(value);
                },
                ResultValue::Done(error)=>{
                    self.pending=false;self.last_refresh=Some(Instant::now());
                    if let Some(error)=error{return self.retry(zsdesk_core::tr_format!("大小计算未完成：{error}", error=error));}
                }
            }
        }
        if changed{self.order_dirty=true;invalidate(&view);}
        if self.order_dirty&&(!self.pending||self.last_order.is_none_or(|last|last.elapsed()>=Duration::from_millis(500))){
            if let Err(error)=folder_totals::refresh_order(&view){return self.retry(zsdesk_core::tr_format!("总大小排序未完成：{error}", error=error));}
            self.order_dirty=false;self.last_order=Some(Instant::now());
        }
        let known=self.values.iter().filter(|value|value.as_ref().is_some_and(|value|value.bytes.is_some())).count();
        let done=self.values.iter().filter(|value|value.is_some()).count();let skipped_links=self.values.iter().filter_map(|value|value.as_ref()).map(|value|value.skipped_links).sum();
        self.status=if self.pending||self.dirty.is_some(){Status::Pending{done,total:self.rows.len()}}else{Status::Ready{known,unknown:self.rows.len()-known,skipped_links}};
        None
    }
    fn retry(&mut self,message:String)->Option<String>{
        self.failures=self.failures.saturating_add(1);self.retry_after=Some(Instant::now()+Duration::from_millis(if self.failures<3{250}else{5000}));
        self.last_refresh=None;self.pending=false;self.gather=None;
        if self.failures<2{self.status=Status::Reading;None}else{self.unavailable(message)}
    }
}
impl Drop for FolderSizes{fn drop(&mut self){self.cancel();}}
fn register_changes(view:&IShellView,folder:&IFolderView2)->Option<ChangeNotify>{
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor,GA_ROOT};
    let persist:IPersistFolder2=unsafe{folder.GetFolder()}.ok()?;let pidl=unsafe{persist.GetCurFolder()}.ok()?;if pidl.is_null(){return None;}
    let length=unsafe{ILGetSize(Some(pidl))}as usize;let bytes=if (2..=65536).contains(&length){Some(unsafe{std::slice::from_raw_parts(pidl.cast::<u8>(),length)}.to_vec())}else{None};
    unsafe{CoTaskMemFree(Some(pidl.cast()))};let owner=unsafe{GetAncestor(view.GetWindow().ok()?,GA_ROOT)};
    ChangeNotify::register(owner,WM_CHANGED,&[bytes?],true)
}
fn invalidate(view:&IShellView){if let Ok(hwnd)=unsafe{view.GetWindow()}{let _=unsafe{InvalidateRect(Some(hwnd),None,false)};}}
fn interaction_active(view:&IShellView)->bool{
    use windows::Win32::{Foundation::{POINT,RECT},UI::{Input::KeyboardAndMouse::{GetFocus,GetAsyncKeyState,VK_LBUTTON,VK_RBUTTON},WindowsAndMessaging::{GetCursorPos,GetWindowRect,IsChild,GetClassNameW}}};
    let Ok(window)=(unsafe{view.GetWindow()})else{return false;};let focus=unsafe{GetFocus()};
    if unsafe{IsChild(window,focus)}.as_bool(){let mut class=[0u16;64];let count=unsafe{GetClassNameW(focus,&mut class)}.max(0)as usize;
        if String::from_utf16_lossy(&class[..count]).eq_ignore_ascii_case("Edit"){return true;}}
    if unsafe{GetAsyncKeyState(VK_LBUTTON.0 as i32)}>=0&&unsafe{GetAsyncKeyState(VK_RBUTTON.0 as i32)}>=0{return false;}
    let mut point=POINT::default();let mut rect=RECT::default();
    unsafe{GetCursorPos(&mut point)}.is_ok()&&unsafe{GetWindowRect(window,&mut rect)}.is_ok()&&point.x>=rect.left&&point.x<rect.right&&point.y>=rect.top&&point.y<rect.bottom
}
fn details_with_total_column(view:&IShellView,folder:&IFolderView2)->bool{
    let mut mode=FOLDERVIEWMODE(0);let mut size=0;if unsafe{folder.GetViewModeAndIconSize(&mut mode,&mut size)}.is_err()||mode!=FVM_DETAILS{return false;}
    let Ok(columns)=view.cast::<IColumnManager>()else{return false;};let Ok(count)=(unsafe{columns.GetColumnCount(CM_ENUM_VISIBLE)})else{return false;};
    if count==0||count>128{return false;}let mut keys=vec![PKEY_TOTAL_SIZE;count as usize];
    unsafe{columns.GetColumns(CM_ENUM_VISIBLE,&mut keys)}.is_ok()&&keys.contains(&PKEY_TOTAL_SIZE)
}
fn write_size(view:&IFolderView2,pidl:&ChildPidl,size:u64)->windows::core::Result<()>{
    let value=Variant::size(size);unsafe{view.SetViewProperty(pidl.pointer(),&PKEY_TOTAL_SIZE,&value.0)}?;
    let readback=Variant(unsafe{view.GetViewProperty(pidl.pointer(),&PKEY_TOTAL_SIZE)}?);
    if unsafe{PropVariantToUInt64(&readback.0)}?!=size{return Err(error(zsdesk_core::i18n::text("总大小缓存校验失败"),0x80004005));}Ok(())
}
#[cfg(test)]#[path="folder_sizes_tests.rs"]mod tests;
