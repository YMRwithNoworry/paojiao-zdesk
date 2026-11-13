//! File clipboard snapshots and bounded asynchronous native batch operations.
//! Common file actions never populate a third-party context menu.
use std::{collections::{HashSet,VecDeque},path::{Path,PathBuf},sync::{Arc,Mutex,OnceLock,atomic::{AtomicBool,Ordering}},thread::{self,JoinHandle}};
use windows::{core::{implement,w,Error,HRESULT,Interface,PCWSTR,Result},Win32::{
    Foundation::{GlobalFree,HANDLE,HGLOBAL,HWND,LPARAM,WPARAM},
    System::{Com::{CoCreateInstance,CoTaskMemFree,CLSCTX_INPROC_SERVER},
        DataExchange::{CloseClipboard,EmptyClipboard,GetClipboardData,GetClipboardOwner,GetClipboardSequenceNumber,IsClipboardFormatAvailable,OpenClipboard,RegisterClipboardFormatW,SetClipboardData},
        Memory::{GlobalAlloc,GlobalLock,GlobalSize,GlobalUnlock,GMEM_MOVEABLE},
        Ole::{OleInitialize,OleUninitialize},Threading::{GetCurrentThreadId,GetCurrentProcessId}},
    UI::{Shell::{*,Common::ITEMIDLIST},WindowsAndMessaging::{GetAncestor,GetWindowThreadProcessId,IsWindow,PostMessageW,RegisterWindowMessageW,GA_ROOT}}}};

const MAX_ITEMS:usize=10000;
const MAX_PATH_BYTES:usize=16*1024*1024;
static WORKERS:Mutex<Vec<JoinHandle<()>>>=Mutex::new(Vec::new());
static QUEUE:Mutex<VecDeque<Request>>=Mutex::new(VecDeque::new());
static RUNNING:AtomicBool=AtomicBool::new(false);
static CREATED:Mutex<VecDeque<(isize,u32,PathBuf)>>=Mutex::new(VecDeque::new());
fn invalid(message:&str)->Error{Error::new(HRESULT(0x80070057u32 as i32),message)}
fn wide(path:&Path)->Vec<u16>{use std::os::windows::ffi::OsStrExt;path.as_os_str().encode_wide().chain(Some(0)).collect()}
pub fn result_message()->u32{static ID:OnceLock<u32>=OnceLock::new();*ID.get_or_init(||unsafe{RegisterWindowMessageW(w!("ZSDesk.FileTransfer.Result.v1"))})}

/// The shared application host keeps its first STA alive until these finish.
pub fn pending_operations()->usize{
    let mut workers=WORKERS.lock().unwrap_or_else(|error|error.into_inner());let mut index=0;
    while index<workers.len(){if workers[index].is_finished(){let _=workers.swap_remove(index).join();}else{index+=1;}}
    workers.len()
}

struct Clipboard;
impl Clipboard{fn open(owner:Option<HWND>)->Result<Self>{unsafe{OpenClipboard(owner)?;}Ok(Self)}}
impl Drop for Clipboard{fn drop(&mut self){let _=unsafe{CloseClipboard()};}}
struct Memory(HGLOBAL);
impl Drop for Memory{fn drop(&mut self){if !self.0.0.is_null(){let _=unsafe{GlobalFree(Some(self.0))};}}}
impl Memory{
    fn new(bytes:&[u8])->Result<Self>{
        let value=Self(unsafe{GlobalAlloc(GMEM_MOVEABLE,bytes.len())}?);
        let target=unsafe{GlobalLock(value.0)};
        if target.is_null(){return Err(Error::from_thread());}
        unsafe{std::ptr::copy_nonoverlapping(bytes.as_ptr(),target.cast(),bytes.len());let _=GlobalUnlock(value.0);}
        Ok(value)
    }
    fn publish(mut self,format:u32)->Result<()>{unsafe{SetClipboardData(format,Some(HANDLE(self.0.0)))?;}self.0=HGLOBAL::default();Ok(())}
}
fn check_paths(paths:&[PathBuf])->Result<()>{
    if paths.is_empty()||paths.len()>MAX_ITEMS{return Err(invalid("文件选择为空或超过 10000 项"));}
    let mut bytes=0usize;
    for path in paths{
        let units=wide(path);
        if !path.is_absolute()||units.len()>32768||units[..units.len()-1].contains(&0){return Err(invalid("文件路径无效"));}
        bytes=bytes.checked_add(units.len()*2).ok_or_else(||invalid("文件路径总量过大"))?;
        if bytes>MAX_PATH_BYTES{return Err(invalid("文件路径总量过大"));}
    }Ok(())
}
fn hdrop_bytes(paths:&[PathBuf])->Result<Vec<u8>>{
    check_paths(paths)?;
    let mut bytes=vec![0u8;20];bytes[..4].copy_from_slice(&20u32.to_le_bytes());bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    for path in paths{for unit in wide(path){bytes.extend(unit.to_le_bytes());}}
    bytes.extend([0,0]);Ok(bytes)
}
pub fn copy_paths(owner:HWND,paths:&[PathBuf],cut:bool)->Result<()>{
    // Both allocations complete before replacing the user's clipboard.
    let names=Memory::new(&hdrop_bytes(paths)?)?;
    let effect=Memory::new(&(if cut{2u32}else{1u32}).to_le_bytes())?;
    let format=unsafe{RegisterClipboardFormatW(w!("Preferred DropEffect"))};if format==0{return Err(Error::from_thread());}
    let _clipboard=Clipboard::open(Some(owner))?;unsafe{EmptyClipboard()?;}
    names.publish(15)?;effect.publish(format)
}
pub fn set_text(owner:HWND,text:&str)->Result<()>{
    if text.contains('\0')||text.len()>MAX_PATH_BYTES{return Err(invalid("剪贴板文字过长或含无效字符"));}
    let bytes=text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    let memory=Memory::new(&bytes)?;
    let _clipboard=Clipboard::open(Some(owner))?;unsafe{EmptyClipboard()?;}memory.publish(13)
}

fn owned_clipboard_on_current_thread()->bool{
    let Ok(owner)=(unsafe{GetClipboardOwner()})else{return false;};let mut process=0;
    let thread=unsafe{GetWindowThreadProcessId(owner,Some(&mut process))};
    process==unsafe{GetCurrentProcessId()}&&thread==unsafe{GetCurrentThreadId()}
}
/// Persist only this STA's filesystem clipboard. Never request CF_FILECONTENTS
/// or flush arbitrary OLE formats; virtual-file providers keep their STA alive.
pub fn persist_owned_file_clipboard(owner:HWND)->Result<bool>{
    let mut process=0;
    if unsafe{GetWindowThreadProcessId(owner,Some(&mut process))}!=unsafe{GetCurrentThreadId()}
        ||process!=unsafe{GetCurrentProcessId()}||!owned_clipboard_on_current_thread(){return Ok(false);}
    if unsafe{IsClipboardFormatAvailable(15)}.is_err(){return Ok(false);}
    let _clipboard=Clipboard::open(Some(owner))?;
    if !owned_clipboard_on_current_thread(){return Ok(false);}
    let (paths,effect,sequence)=clipboard_files_locked()?;
    let names=Memory::new(&hdrop_bytes(&paths)?)?;
    let preferred=Memory::new(&effect.to_le_bytes())?;
    let format=unsafe{RegisterClipboardFormatW(w!("Preferred DropEffect"))};if format==0{return Err(Error::from_thread());}
    if unsafe{GetClipboardSequenceNumber()}!=sequence||!owned_clipboard_on_current_thread(){return Ok(false);}
    unsafe{EmptyClipboard()?;}names.publish(15)?;preferred.publish(format)?;Ok(true)
}
pub fn selected_paths(view:&IShellView)->Result<Vec<PathBuf>>{
    let folder:IFolderView2=view.cast()?;let items:IShellItemArray=unsafe{folder.Items(SVGIO_SELECTION)?};
    let count=unsafe{items.GetCount()?}as usize;
    if count==0{return Ok(Vec::new());}if count>MAX_ITEMS{return Err(invalid("选择超过 10000 项"));}
    let mut paths=Vec::with_capacity(count);
    for index in 0..count{let item=unsafe{items.GetItemAt(index as u32)?};paths.push(item_path(&item)?);}
    check_paths(&paths)?;Ok(paths)
}
fn item_path(item:&IShellItem)->Result<PathBuf>{
    let value=unsafe{item.GetDisplayName(SIGDN_FILESYSPATH)?};let text=unsafe{value.to_string()};unsafe{CoTaskMemFree(Some(value.0.cast()));}Ok(text.map(PathBuf::from)?)
}
pub fn folder_path(view:&IShellView)->Result<PathBuf>{
    let folder:IFolderView2=view.cast()?;let persistent:IPersistFolder2=unsafe{folder.GetFolder()?};
    let pidl=unsafe{persistent.GetCurFolder()?};let result=unsafe{SHCreateItemFromIDList::<IShellItem>(pidl)};unsafe{CoTaskMemFree(Some(pidl.cast()));}item_path(&result?)
}
pub fn copy_view(view:&IShellView,owner:HWND,cut:bool)->Result<()>{
    let paths=selected_paths(view)?;if paths.is_empty(){return Ok(());}copy_paths(owner,&paths,cut)
}
pub fn paths_from_pidls(pidls:&[Vec<u8>])->Result<Vec<PathBuf>>{
    if pidls.len()>MAX_ITEMS{return Err(invalid("选择超过 10000 项"));}
    let mut paths=Vec::with_capacity(pidls.len());
    for pidl in pidls{paths.push(item_path(&super::pidl::item_from_pidl(pidl)?)?);}check_paths(&paths)?;Ok(paths)
}
pub fn copy_items(owner:HWND,pidls:&[Vec<u8>],cut:bool)->Result<()>{copy_paths(owner,&paths_from_pidls(pidls)?,cut)}

fn clipboard_files()->Result<(Vec<PathBuf>,u32,u32)>{
    // Keep sequence and move intent from one locked clipboard snapshot.
    let _clipboard=Clipboard::open(None)?;clipboard_files_locked()
}
fn clipboard_files_locked()->Result<(Vec<PathBuf>,u32,u32)>{
    let sequence=unsafe{GetClipboardSequenceNumber()};
    let handle=unsafe{GetClipboardData(15)}?;let memory=HGLOBAL(handle.0);
    let bytes=unsafe{GlobalSize(memory)};
    if bytes<22||bytes>MAX_PATH_BYTES+22{return Err(invalid("剪贴板文件清单长度无效"));}
    let pointer=unsafe{GlobalLock(memory)};if pointer.is_null(){return Err(Error::from_thread());}
    let paths=parse_hdrop(unsafe{std::slice::from_raw_parts(pointer.cast::<u8>(),bytes)});
    let _=unsafe{GlobalUnlock(memory)};let paths=paths?;
    let format=unsafe{RegisterClipboardFormatW(w!("Preferred DropEffect"))};if format==0{return Err(Error::from_thread());}let mut effect=1u32;
    if let Ok(handle)=unsafe{GetClipboardData(format)}{let memory=HGLOBAL(handle.0);
        if unsafe{GlobalSize(memory)}>=4{let pointer=unsafe{GlobalLock(memory)};
            if !pointer.is_null(){effect=unsafe{pointer.cast::<u32>().read_unaligned()}&7;let _=unsafe{GlobalUnlock(memory)};}
        }
    }
    check_paths(&paths)?;Ok((paths,effect,sequence))
}

/// Scan the bounded path list once. Repeated DragQueryFileW(index) rescans all
/// preceding strings and becomes quadratic for large multi-file selections.
fn parse_hdrop(bytes:&[u8])->Result<Vec<PathBuf>>{
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Globalization::{MultiByteToWideChar,CP_ACP,MB_ERR_INVALID_CHARS};
    if bytes.len()<22||bytes.len()>MAX_PATH_BYTES+22{return Err(invalid("剪贴板文件清单长度无效"));}
    let offset=u32::from_le_bytes(bytes[..4].try_into().unwrap())as usize;
    let unicode=u32::from_le_bytes(bytes[16..20].try_into().unwrap())!=0;
    if offset<20||offset>=bytes.len()||(unicode&&offset%2!=0){return Err(invalid("剪贴板文件清单偏移无效"));}
    let mut paths=Vec::new();let mut cursor=offset;let mut total=0usize;
    loop{
        let start=cursor;
        let units=if unicode{
            while cursor.checked_add(2).is_some_and(|end|end<=bytes.len()){
                if bytes[cursor]==0&&bytes[cursor+1]==0{break;}
                cursor+=2;
            }
            if cursor+2>bytes.len(){return Err(invalid("剪贴板文件清单缺少结束符"));}
            let length=(cursor-start)/2;cursor+=2;
            if length==0{if paths.is_empty(){return Err(invalid("剪贴板文件清单为空"));}return Ok(paths);}
            if length>32767{return Err(invalid("剪贴板文件路径过长"));}
            bytes[start..cursor-2].chunks_exact(2).map(|pair|u16::from_le_bytes([pair[0],pair[1]])).collect::<Vec<_>>()
        }else{
            while cursor<bytes.len()&&bytes[cursor]!=0{cursor+=1;}
            if cursor==bytes.len(){return Err(invalid("剪贴板文件清单缺少结束符"));}
            let length=cursor-start;cursor+=1;
            if length==0{if paths.is_empty(){return Err(invalid("剪贴板文件清单为空"));}return Ok(paths);}
            if length>65534{return Err(invalid("剪贴板文件路径过长"));}
            let input=&bytes[start..cursor-1];let count=unsafe{MultiByteToWideChar(CP_ACP,MB_ERR_INVALID_CHARS,input,None)};
            if count<=0||count>32767{return Err(invalid("剪贴板 ANSI 文件路径无效"));}
            let mut units=vec![0u16;count as usize];
            if unsafe{MultiByteToWideChar(CP_ACP,MB_ERR_INVALID_CHARS,input,Some(&mut units))}!=count{return Err(invalid("剪贴板 ANSI 文件路径转换失败"));}units
        };
        total=total.checked_add((units.len()+1)*2).ok_or_else(||invalid("剪贴板文件路径总量过大"))?;
        if total>MAX_PATH_BYTES||paths.len()>=MAX_ITEMS{return Err(invalid("剪贴板文件清单过大或超过 10000 项"));}
        let path=PathBuf::from(std::ffi::OsString::from_wide(&units));
        if !path.is_absolute(){return Err(invalid("剪贴板文件路径必须为绝对路径"));}
        paths.push(path);
    }
}

#[derive(Clone,Copy,Debug)]pub enum Operation{Copy,Move,Delete{permanent:bool},NewFolder}
#[derive(Clone,Debug)]struct Request{owner:isize,thread:u32,paths:Vec<PathBuf>,destination:Option<PathBuf>,operation:Operation,clipboard:Option<u32>}
pub fn paste(owner:HWND,destination:&Path)->Result<()>{
    // An opened .zip reports its own file path; only its Shell folder can paste into it.
    if !destination.is_absolute()||!destination.is_dir(){return Err(invalid("目标文件夹无效"));}
    let (paths,effect,sequence)=clipboard_files()?;let moving=effect&2!=0;
    submit(owner,paths,Some(destination.into()),if moving{Operation::Move}else{Operation::Copy},moving.then_some(sequence))
}
pub fn paste_view(view:&IShellView,owner:HWND)->Result<()>{paste(owner,&folder_path(view)?)}
pub fn delete_view(view:&IShellView,owner:HWND,permanent:bool)->Result<()>{
    let paths=selected_paths(view)?;if paths.is_empty(){return Ok(());}delete_paths(owner,paths,permanent)
}
pub fn delete_paths(owner:HWND,paths:Vec<PathBuf>,permanent:bool)->Result<()>{submit(owner,paths,None,Operation::Delete{permanent},None)}
pub fn transfer_paths(owner:HWND,paths:Vec<PathBuf>,destination:PathBuf,moving:bool)->Result<()>{submit(owner,paths,Some(destination),if moving{Operation::Move}else{Operation::Copy},None)}
pub fn new_folder(owner:HWND,destination:&Path)->Result<()>{submit(owner,Vec::new(),Some(destination.into()),Operation::NewFolder,None)}
pub fn take_created_folder(owner:HWND)->Option<PathBuf>{
    let root=unsafe{GetAncestor(owner,GA_ROOT)};let owner=if root.0.is_null(){owner}else{root};
    let thread=unsafe{GetWindowThreadProcessId(owner,None)};
    let mut created=CREATED.lock().unwrap_or_else(|error|error.into_inner());
    let index=created.iter().position(|(window,id,_)|*window==owner.0 as isize&&*id==thread)?;
    created.remove(index).map(|(_,_,path)|path)
}
fn submit(owner:HWND,paths:Vec<PathBuf>,destination:Option<PathBuf>,operation:Operation,clipboard:Option<u32>)->Result<()>{
    if !matches!(operation,Operation::NewFolder){check_paths(&paths)?;}
    if let Some(destination)=&destination{check_paths(&[destination.clone()])?;}
    let root=unsafe{GetAncestor(owner,GA_ROOT)};let owner=if root.0.is_null(){owner}else{root};
    let request=Request{owner:owner.0 as isize,thread:unsafe{GetWindowThreadProcessId(owner,None)},paths,destination,operation,clipboard};
    let message=result_message();pending_operations();
    let mut workers=WORKERS.lock().unwrap_or_else(|error|error.into_inner());
    let mut queue=QUEUE.lock().unwrap_or_else(|error|error.into_inner());
    if queue.len()+usize::from(RUNNING.load(Ordering::Acquire))>=16{return Err(invalid("文件操作队列已满，请等待当前操作完成"));}
    queue.push_back(request);
    if RUNNING.load(Ordering::Acquire){return Ok(());}
    RUNNING.store(true,Ordering::Release);
    let worker=thread::Builder::new().name("zsdesk-file-operation".into()).spawn(move||{
        loop{
            let request={let mut queue=QUEUE.lock().unwrap_or_else(|error|error.into_inner());
                match queue.pop_front(){Some(request)=>request,None=>{RUNNING.store(false,Ordering::Release);break;}}};
            let result=std::panic::catch_unwind(||perform(&request,false)).unwrap_or_else(|_|Err(Error::from_hresult(HRESULT(0x80004005u32 as i32))));
            let owner=HWND(request.owner as*mut _);
            if unsafe{IsWindow(Some(owner))}.as_bool()&&unsafe{GetWindowThreadProcessId(owner,None)}==request.thread{
                let code=result.map_or_else(|error|error.code().0 as u32,|outcome|{
                    if let Some(path)=outcome.created{let mut created=CREATED.lock().unwrap_or_else(|error|error.into_inner());
                        created.retain(|(window,thread,_)|unsafe{IsWindow(Some(HWND(*window as*mut _)))}.as_bool()&&unsafe{GetWindowThreadProcessId(HWND(*window as*mut _),None)}==*thread);
                        if created.len()>=32{created.pop_front();}created.push_back((request.owner,request.thread,path));}
                    if outcome.aborted{0x800704c7}else{0}
                });
                let _=unsafe{PostMessageW(Some(owner),message,WPARAM(code as usize),LPARAM(0))};
            }
        }
    });
    let worker=match worker{Ok(worker)=>worker,Err(error)=>{RUNNING.store(false,Ordering::Release);queue.pop_back();return Err(Error::new(HRESULT(0x80004005u32 as i32),error.to_string()));}};
    workers.push(worker);Ok(())
}
fn completion_key(path:&Path)->Vec<u16>{
    use std::os::windows::ffi::OsStrExt;
    // Preserve every non-ASCII UTF-16 unit. This is deliberately narrower than
    // filesystem-wide Unicode case folding; uncertain matches remain pending.
    path.as_os_str().encode_wide().map(|unit|match unit{
        65..=90=>unit+32,47=>92,_=>unit,
    }).collect()
}
#[derive(Default)]struct ProgressState{
    finished:Option<HRESULT>,failure:Option<HRESULT>,skipped:bool,
    expected:HashSet<Vec<u16>>,completed:HashSet<Vec<u16>>,created:Option<PathBuf>,
}
impl ProgressState{
    fn record_completed(&mut self,path:&Path){
        let key=completion_key(path);
        if self.expected.contains(&key){self.completed.insert(key);}
    }
}
#[implement(IFileOperationProgressSink)]struct Progress{state:Arc<Mutex<ProgressState>>,cancel_before:bool}
impl Progress{
    fn before(&self)->Result<()>{if self.cancel_before{Err(Error::from_hresult(COPYENGINE_E_CANCELLED))}else{Ok(())}}
    fn after(&self,item:windows::core::Ref<IShellItem>,result:HRESULT,created:windows::core::Ref<IShellItem>,new:bool)->Result<()>{
        let mut state=self.state.lock().unwrap_or_else(|error|error.into_inner());
        if result.is_err(){state.failure.get_or_insert(result);return Ok(());}
        if [COPYENGINE_S_USER_IGNORED,COPYENGINE_S_NOT_HANDLED,COPYENGINE_S_PENDING,COPYENGINE_S_PENDING_DELETE].contains(&result)||result==HRESULT(1){state.skipped=true;return Ok(());}
        // Recursive child results still report failures/skips above, but only
        // the bounded top-level request set contributes retained completion IDs.
        if let Some(item)=item.as_ref(){state.record_completed(&item_path(item)?);}
        if new{state.created=created.as_ref().map(item_path).transpose()?;if state.created.is_none(){state.skipped=true;}}
        Ok(())
    }
}
#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for Progress_Impl{
    fn StartOperations(&self)->Result<()>{self.before()}
    fn FinishOperations(&self,result:HRESULT)->Result<()>{self.state.lock().unwrap_or_else(|error|error.into_inner()).finished=Some(result);Ok(())}
    fn PreRenameItem(&self,_:u32,_:windows::core::Ref<IShellItem>,_:&PCWSTR)->Result<()>{self.before()}
    fn PostRenameItem(&self,_:u32,item:windows::core::Ref<IShellItem>,_:&PCWSTR,result:HRESULT,created:windows::core::Ref<IShellItem>)->Result<()>{self.after(item,result,created,false)}
    fn PreMoveItem(&self,_:u32,_:windows::core::Ref<IShellItem>,_:windows::core::Ref<IShellItem>,_:&PCWSTR)->Result<()>{self.before()}
    fn PostMoveItem(&self,_:u32,item:windows::core::Ref<IShellItem>,_:windows::core::Ref<IShellItem>,_:&PCWSTR,result:HRESULT,created:windows::core::Ref<IShellItem>)->Result<()>{self.after(item,result,created,false)}
    fn PreCopyItem(&self,_:u32,_:windows::core::Ref<IShellItem>,_:windows::core::Ref<IShellItem>,_:&PCWSTR)->Result<()>{self.before()}
    fn PostCopyItem(&self,_:u32,item:windows::core::Ref<IShellItem>,_:windows::core::Ref<IShellItem>,_:&PCWSTR,result:HRESULT,created:windows::core::Ref<IShellItem>)->Result<()>{self.after(item,result,created,false)}
    fn PreDeleteItem(&self,_:u32,_:windows::core::Ref<IShellItem>)->Result<()>{self.before()}
    fn PostDeleteItem(&self,_:u32,item:windows::core::Ref<IShellItem>,result:HRESULT,created:windows::core::Ref<IShellItem>)->Result<()>{self.after(item,result,created,false)}
    fn PreNewItem(&self,_:u32,_:windows::core::Ref<IShellItem>,_:&PCWSTR)->Result<()>{self.before()}
    fn PostNewItem(&self,_:u32,_:windows::core::Ref<IShellItem>,_:&PCWSTR,_:&PCWSTR,_:u32,result:HRESULT,created:windows::core::Ref<IShellItem>)->Result<()>{self.after(windows::core::Ref::default(),result,created,true)}
    fn UpdateProgress(&self,_:u32,_:u32)->Result<()>{self.before()}
    fn ResetTimer(&self)->Result<()>{Ok(())}fn PauseTimer(&self)->Result<()>{Ok(())}fn ResumeTimer(&self)->Result<()>{Ok(())}
}
#[derive(Debug)]struct Outcome{aborted:bool,created:Option<PathBuf>}
fn perform(request:&Request,quiet:bool)->Result<Outcome>{perform_inner(request,quiet,false)}
fn perform_inner(request:&Request,quiet:bool,cancel_before:bool)->Result<Outcome>{
    if !matches!(request.operation,Operation::NewFolder){check_paths(&request.paths)?;}
    unsafe{OleInitialize(None)?;}struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let operation:IFileOperation=unsafe{CoCreateInstance(&FileOperation,None,CLSCTX_INPROC_SERVER)?};
    let mut flags=FOF_NOCONFIRMMKDIR|FOF_ALLOWUNDO|FOFX_ADDUNDORECORD;
    match request.operation{Operation::Delete{permanent:false}=>flags|=FOFX_RECYCLEONDELETE|FOF_WANTNUKEWARNING,
        Operation::Delete{permanent:true}=>{flags=FOF_WANTNUKEWARNING;},_=>{}}
    if quiet{flags|=FOF_SILENT|FOF_NOERRORUI|FOFX_EARLYFAILURE|FOF_NOCONFIRMATION;}
    unsafe{operation.SetOperationFlags(flags)?;}
    let progress=Arc::new(Mutex::new(ProgressState{expected:request.paths.iter().map(|path|completion_key(path)).collect(),..Default::default()}));
    let sink:IFileOperationProgressSink=Progress{state:progress.clone(),cancel_before}.into();
    let cookie=unsafe{operation.Advise(&sink)}?;
    struct Advisory<'a>{operation:&'a IFileOperation,cookie:u32}impl Drop for Advisory<'_>{fn drop(&mut self){let _=unsafe{self.operation.Unadvise(self.cookie)};}}let _advisory=Advisory{operation:&operation,cookie};
    let owner=HWND(request.owner as*mut _);
    if unsafe{IsWindow(Some(owner))}.as_bool()&&unsafe{GetWindowThreadProcessId(owner,None)}==request.thread{unsafe{operation.SetOwnerWindow(owner)?;}}
    let mut identities=Vec::with_capacity(request.paths.len());
    for path in &request.paths{let item:IShellItem=unsafe{SHCreateItemFromParsingName(PCWSTR(wide(path).as_ptr()),None)?};
        identities.push(super::pidl::item_pidl(&item).ok_or_else(||invalid("无法创建文件标识"))?);}
    let pointers=identities.iter().map(|pidl|pidl.as_ptr().cast::<ITEMIDLIST>()).collect::<Vec<_>>();
    let items=if pointers.is_empty(){None}else{Some(unsafe{SHCreateShellItemArrayFromIDLists(&pointers)?})};
    match request.operation{
        Operation::Copy|Operation::Move=>{
            let destination=request.destination.as_ref().ok_or_else(||invalid("缺少目标文件夹"))?;
            let target:IShellItem=unsafe{SHCreateItemFromParsingName(PCWSTR(wide(destination).as_ptr()),None)?};
            let items=items.as_ref().ok_or_else(||invalid("文件选择为空"))?;
            if matches!(request.operation,Operation::Move){unsafe{operation.MoveItems(items,&target)?;}}else{unsafe{operation.CopyItems(items,&target)?;}}
        },Operation::Delete{..}=>unsafe{operation.DeleteItems(items.as_ref().ok_or_else(||invalid("文件选择为空"))?)?;},
        Operation::NewFolder=>{
            let destination=request.destination.as_ref().ok_or_else(||invalid("缺少目标文件夹"))?;
            let target:IShellItem=unsafe{SHCreateItemFromParsingName(PCWSTR(wide(destination).as_ptr()),None)?};
            let mut name="新建文件夹".to_owned();
            for index in 1..=10000{
                if !destination.join(&name).try_exists().map_err(|error|invalid(&error.to_string()))?{break;}
                if index==10000{return Err(invalid("同名文件夹过多，请指定名称"));}name=format!("新建文件夹 ({})",index+1);
            }
            // Name availability is checked on this worker, including network
            // folders; IFileOperation owns any subsequent race/collision UI.
            unsafe{operation.NewItem(&target,windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY.0,PCWSTR(wide(Path::new(&name)).as_ptr()),PCWSTR::null(),None)?;}
        }
    }
    let performed=unsafe{operation.PerformOperations()};
    let mut aborted=unsafe{operation.GetAnyOperationsAborted()?}.as_bool();
    let state=progress.lock().unwrap_or_else(|error|error.into_inner());
    let failure=performed.err().map(|error|error.code()).or(state.failure).or(state.finished.filter(|result|result.is_err()));
    if let Some(failure)=failure{if failure==COPYENGINE_E_CANCELLED||failure==HRESULT(0x800704c7u32 as i32){aborted=true;}else{return Err(Error::from_hresult(failure));}}
    aborted|=state.skipped||state.finished.is_none();
    if matches!(request.operation,Operation::NewFolder){aborted|=state.created.is_none();}
    else{aborted|=state.completed.len()!=state.expected.len();}
    let created=(!aborted).then(||state.created.clone()).flatten();
    drop(state);
    if !aborted{if let Some(sequence)=request.clipboard{
        if let Ok(_clipboard)=Clipboard::open(None){if unsafe{GetClipboardSequenceNumber()}==sequence{let _=unsafe{EmptyClipboard()};}}
    }}
    Ok(Outcome{aborted,created})
}

#[cfg(test)]#[path="file_transfer_tests.rs"]mod tests;
