//! File clipboard snapshots and bounded asynchronous native batch operations.
//! Common file actions never populate a third-party context menu.
use std::{collections::{HashMap,HashSet,VecDeque},path::{Path,PathBuf},sync::{Arc,Mutex,OnceLock,mpsc,atomic::{AtomicU64,AtomicUsize,Ordering}},thread::{self,JoinHandle}};
use windows::{core::{implement,w,Error,HRESULT,Interface,PCWSTR,Result},Win32::{
    Foundation::{CloseHandle,GlobalFree,HANDLE,HGLOBAL,HWND,LPARAM,WPARAM},
    System::{Com::{CoCreateInstance,CoTaskMemFree,CoWaitForMultipleHandles,CLSCTX_INPROC_SERVER,COWAIT_DISPATCH_CALLS,COWAIT_INPUTAVAILABLE},
        DataExchange::{CloseClipboard,EmptyClipboard,GetClipboardData,GetClipboardOwner,GetClipboardSequenceNumber,IsClipboardFormatAvailable,OpenClipboard,RegisterClipboardFormatW,SetClipboardData},
        Memory::{GlobalAlloc,GlobalLock,GlobalSize,GlobalUnlock,GMEM_MOVEABLE},
        Ole::{OleInitialize,OleUninitialize},SystemServices::SFGAO_FILESYSTEM,Threading::{CreateEventW,SetEvent,GetCurrentThreadId,GetCurrentProcessId}},
    UI::{Shell::{*,Common::ITEMIDLIST},WindowsAndMessaging::{GetAncestor,GetParent,GetPropW,SetPropW,RemovePropW,GetWindowThreadProcessId,IsWindow,PostMessageW,RegisterWindowMessageW,
        PeekMessageW,TranslateMessage,DispatchMessageW,PostQuitMessage,MsgWaitForMultipleObjectsEx,MSG,PM_REMOVE,WM_QUIT,QS_ALLINPUT,MWMO_INPUTAVAILABLE,GA_ROOT}}}};

const MAX_ITEMS:usize=10000;
const MAX_PATH_BYTES:usize=16*1024*1024;
static WORKERS:Mutex<Vec<JoinHandle<()>>>=Mutex::new(Vec::new());
struct TransferQueue{waiting:VecDeque<Request>,active:Vec<Request>,workers:usize}
static QUEUE:Mutex<TransferQueue>=Mutex::new(TransferQueue{waiting:VecDeque::new(),active:Vec::new(),workers:0});
const MAX_WORKERS:usize=4;
static CREATED:Mutex<VecDeque<(isize,u32,PathBuf)>>=Mutex::new(VecDeque::new());
static NEXT_OPERATION:AtomicUsize=AtomicUsize::new(1);
static COMPLETIONS:Mutex<VecDeque<(usize,isize,u32,Completion)>>=Mutex::new(VecDeque::new());
/// A view's navigation identity is data, never a COM pointer or an HWND alone.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct TransferOrigin{pub view:isize,pub pane:usize,pub tab:u64,pub epoch:u64,pub activity:u64}
static NEXT_ACTIVITY:AtomicU64=AtomicU64::new(1);
const ORIGIN_VALID:PCWSTR=w!("ZSDesk.TransferOrigin.Valid");
const ORIGIN_PANE:PCWSTR=w!("ZSDesk.TransferOrigin.Pane");
const ORIGIN_TAB_LO:PCWSTR=w!("ZSDesk.TransferOrigin.TabLow");
const ORIGIN_TAB_HI:PCWSTR=w!("ZSDesk.TransferOrigin.TabHigh");
const ORIGIN_EPOCH_LO:PCWSTR=w!("ZSDesk.TransferOrigin.EpochLow");
const ORIGIN_EPOCH_HI:PCWSTR=w!("ZSDesk.TransferOrigin.EpochHigh");
const ORIGIN_ACTIVITY_LO:PCWSTR=w!("ZSDesk.TransferOrigin.ActivityLow");
const ORIGIN_ACTIVITY_HI:PCWSTR=w!("ZSDesk.TransferOrigin.ActivityHigh");
pub fn clear_origin(view:HWND){let _=unsafe{RemovePropW(view,ORIGIN_VALID)};}
fn owns_view_thread(view:HWND)->bool{
    let mut process=0;let thread=unsafe{GetWindowThreadProcessId(view,Some(&mut process))};
    thread==unsafe{GetCurrentThreadId()}&&process==unsafe{GetCurrentProcessId()}
}
pub fn bind_origin(view:HWND,pane:usize,tab:u64,epoch:u64)->Result<()>{
    if !unsafe{IsWindow(Some(view))}.as_bool()||!owns_view_thread(view){return Err(invalid("文件视图归属无效"));}
    if capture_origin(view).is_some_and(|origin|origin.pane==pane&&origin.tab==tab&&origin.epoch==epoch){return Ok(());}
    clear_origin(view);
    if pane==0||tab==0{return Err(invalid("文件视图标识无效"));}
    let activity=NEXT_ACTIVITY.fetch_add(1,Ordering::Relaxed).max(1);
    // Commit VALID last. Separate words preserve u64 IDs on every pointer width.
    let values=[(ORIGIN_PANE,pane),(ORIGIN_TAB_LO,tab as u32 as usize),(ORIGIN_TAB_HI,(tab>>32)as usize),
        (ORIGIN_EPOCH_LO,epoch as u32 as usize),(ORIGIN_EPOCH_HI,(epoch>>32)as usize),
        (ORIGIN_ACTIVITY_LO,activity as u32 as usize),(ORIGIN_ACTIVITY_HI,(activity>>32)as usize)];
    for (name,value) in values{unsafe{SetPropW(view,name,Some(HANDLE(value as*mut _)))?;}}
    unsafe{SetPropW(view,ORIGIN_VALID,Some(HANDLE(1usize as*mut _)))}
}
fn capture_origin(view:HWND)->Option<TransferOrigin>{
    if !owns_view_thread(view)||unsafe{GetPropW(view,ORIGIN_VALID)}.0 as usize!=1{return None;}
    let value=|key|unsafe{GetPropW(view,key)}.0 as usize;
    let pane=value(ORIGIN_PANE);let tab=value(ORIGIN_TAB_LO)as u32 as u64|((value(ORIGIN_TAB_HI)as u32 as u64)<<32);
    let epoch=value(ORIGIN_EPOCH_LO)as u32 as u64|((value(ORIGIN_EPOCH_HI)as u32 as u64)<<32);
    let activity=value(ORIGIN_ACTIVITY_LO)as u32 as u64|((value(ORIGIN_ACTIVITY_HI)as u32 as u64)<<32);
    (pane!=0&&tab!=0&&activity!=0&&value(ORIGIN_VALID)==1).then_some(TransferOrigin{view:view.0 as isize,pane,tab,epoch,activity})
}
/// Receives actual input before its action dispatch. No completion may later
/// replace a selection the user changed while an operation was in progress.
pub fn note_user_input(mut target:HWND){
    for _ in 0..64{
        if !owns_view_thread(target){return;}
        if capture_origin(target).is_some(){
            let activity=NEXT_ACTIVITY.fetch_add(1,Ordering::Relaxed).max(1);clear_origin(target);
            let result=(||->Result<()>{unsafe{
                SetPropW(target,ORIGIN_ACTIVITY_LO,Some(HANDLE(activity as u32 as usize as*mut _)))?;
                SetPropW(target,ORIGIN_ACTIVITY_HI,Some(HANDLE((activity>>32)as usize as*mut _)))?;
                SetPropW(target,ORIGIN_VALID,Some(HANDLE(1usize as*mut _)))
            }})();
            if result.is_err(){clear_origin(target);}return;
        }
        let Ok(parent)=(unsafe{GetParent(target)})else{return;};if parent==target{return;}target=parent;
    }
}
pub fn origin_current(origin:&TransferOrigin)->bool{capture_origin(HWND(origin.view as*mut _))==Some(*origin)}
#[derive(Clone,Debug)]pub struct Completion{
    pub operation:Operation,pub requested:usize,pub completed:usize,pub aborted:bool,pub cancelled:bool,pub skipped:bool,
    pub outputs:Vec<PathBuf>,pub outputs_truncated:bool,pub origin:Option<TransferOrigin>,pub error:Option<HRESULT>,pub error_detail:Option<String>,
}
impl Completion{pub fn message(&self)->String{
    let verb=match self.operation{Operation::Copy=>zsdesk_core::i18n::text("复制"),Operation::Move=>"移动",Operation::Delete{..}=>zsdesk_core::i18n::text("删除"),Operation::NewFolder=>"新建文件夹"};
    let message=if let Some(error)=self.error.filter(|error|!is_cancelled(*error)){
        let detail=self.error_detail.clone().unwrap_or_else(||error_message(&Error::from_hresult(error)));
        format!("{verb}未完成：{detail}（已完成 {} / {} 项）",self.completed,self.requested)
    }else if self.aborted{let status=if self.cancelled{"已取消"}else if self.skipped{"已跳过部分项目"}else{"尚未全部完成"};format!("{verb}{status}（已完成 {} / {} 项）",self.completed,self.requested)}
    else{format!("{verb}完成（{} 项）",self.completed)};
    if self.outputs_truncated{format!("{message}；部分结果未自动选中")}else{message}
}
    pub fn result_code(&self)->u32{self.error.map_or(if self.aborted{0x800704c7}else{0},|error|error.0 as u32)}
}
fn invalid(message:&str)->Error{Error::new(HRESULT(0x80070057u32 as i32),zsdesk_core::i18n::text(message))}
fn wide(path:&Path)->Vec<u16>{use std::os::windows::ffi::OsStrExt;path.as_os_str().encode_wide().chain(Some(0)).collect()}
pub fn result_message()->u32{static ID:OnceLock<u32>=OnceLock::new();*ID.get_or_init(||unsafe{RegisterWindowMessageW(w!("ZSDesk.FileTransfer.Result.v1"))})}
/// Shell and Win32 use different HRESULTs for the same explicit cancellation.
pub fn is_cancelled(code:HRESULT)->bool{matches!(code.0 as u32,0x80270000|0x80270001|0x800704c7|0x80004004)}
pub fn error_message(error:&Error)->String{
    let code=error.code();if is_cancelled(code){return zsdesk_core::i18n::text("操作已取消").into();}
    let text=match code.0 as u32{
        0x80070002|0x80070003=>"找不到文件或路径，请刷新后重试".into(),
        0x80070005=>"没有访问此文件或文件夹的权限".into(),
        0x80070020=>"文件正被其他程序占用".into(),
        0x80070070=>"目标磁盘空间不足".into(),
        _ if code==COPYENGINE_E_SAME_FILE=>"源项目和目标是同一文件或文件夹".into(),
        _ if code==COPYENGINE_E_DEST_SUBTREE=>"不能把文件夹复制或移动到它自己的子目录".into(),
        _ if code==COPYENGINE_E_FILE_TOO_LARGE=>"文件超过目标文件系统允许的大小".into(),
        _=>error.message(),
    };
    format!("{} (0x{:08X})",zsdesk_core::i18n::text(&text),code.0 as u32)
}
pub fn has_filesystem_clipboard()->bool{unsafe{IsClipboardFormatAvailable(15)}.is_ok()}
pub fn has_paste_content()->bool{has_filesystem_clipboard()||has_shell_payload()}
fn has_shell_payload()->bool{
    [CFSTR_SHELLIDLIST,CFSTR_FILEDESCRIPTORW,CFSTR_FILEDESCRIPTORA].iter().any(|name|{
        let id=unsafe{RegisterClipboardFormatW(*name)};id!=0&&unsafe{IsClipboardFormatAvailable(id)}.is_ok()
    })
}
pub fn take_completion(owner:HWND,token:usize)->Option<Completion>{
    let thread=unsafe{GetWindowThreadProcessId(owner,None)};let mut queue=COMPLETIONS.lock().unwrap_or_else(|e|e.into_inner());
    let index=queue.iter().position(|(id,window,thread_id,_)|*id==token&&*window==owner.0 as isize&&*thread_id==thread)?;
    queue.remove(index).map(|(_,_,_,value)|value)
}

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
    let data=file_data_object(owner,paths,cut)?;
    unsafe{windows::Win32::System::Ole::OleSetClipboard(&data)}
}
fn file_data_object(owner:HWND,paths:&[PathBuf],cut:bool)->Result<windows::Win32::System::Com::IDataObject>{
    // Shell's IDataObject carries CIDA and delayed descriptor/content formats
    // used by remote clipboard bridges and copy handlers, as well as HDROP.
    check_paths(paths)?;
    let pidls=paths.iter().map(|path|super::pidl::location_pidl(&zsdesk_core::model::explorer::ShellLocation::Path(path.clone()))
        .ok_or_else(||invalid(zsdesk_core::i18n::text("无法读取文件标识")))).collect::<Result<Vec<_>>>()?;
    let data=crate::desktop::data_object(owner,&pidls)?;
    set_drop_effect(&data, cut)?;
    Ok(data)
}

fn set_drop_effect(data: &windows::Win32::System::Com::IDataObject, cut: bool) -> Result<()> {
    let format=unsafe{RegisterClipboardFormatW(w!("Preferred DropEffect"))};if format==0{return Err(Error::from_thread());}
    let mut memory=Memory::new(&(if cut{2u32}else{1u32}).to_le_bytes())?;
    let descriptor=windows::Win32::System::Com::FORMATETC{cfFormat:format as u16,ptd:std::ptr::null_mut(),
        dwAspect:windows::Win32::System::Com::DVASPECT_CONTENT.0,lindex:-1,tymed:windows::Win32::System::Com::TYMED_HGLOBAL.0 as u32};
    let medium=windows::Win32::System::Com::STGMEDIUM{tymed:windows::Win32::System::Com::TYMED_HGLOBAL.0 as u32,
        u:windows::Win32::System::Com::STGMEDIUM_0{hGlobal:memory.0},pUnkForRelease:std::mem::ManuallyDrop::new(None)};
    unsafe{data.SetData(&descriptor,&medium,true)?;}memory.0=HGLOBAL::default();
    Ok(())
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
/// Preserve the complete filesystem data object when its owning STA retires.
/// Virtual-file providers without HDROP retain their own streaming lifetime.
pub fn persist_owned_file_clipboard(owner:HWND)->Result<bool>{
    let mut process=0;
    if unsafe{GetWindowThreadProcessId(owner,Some(&mut process))}!=unsafe{GetCurrentThreadId()}
        ||process!=unsafe{GetCurrentProcessId()}||!owned_clipboard_on_current_thread(){return Ok(false);}
    persist_thread_clipboard()
}
pub fn persist_thread_clipboard()->Result<bool>{
    if !owned_clipboard_on_current_thread(){return Ok(true);}
    if unsafe{IsClipboardFormatAvailable(15)}.is_err(){return Ok(false);}
    // Do not replace the object with HDROP alone: doing so discards formats
    // requested by RDP and other remote clipboard transports.
    unsafe{windows::Win32::System::Ole::OleFlushClipboard()?;}Ok(true)
}
pub fn selected_paths(view:&IShellView)->Result<Vec<PathBuf>>{
    let folder:IFolderView2=view.cast()?;let items:IShellItemArray=unsafe{folder.Items(SVGIO_SELECTION)?};
    let count=unsafe{items.GetCount()?}as usize;
    if count==0{return Ok(Vec::new());}if count>MAX_ITEMS{return Err(invalid("选择超过 10000 项"));}
    let mut paths=Vec::with_capacity(count);
    for index in 0..count{let item=unsafe{items.GetItemAt(index as u32)?};paths.push(item_path(&item)?);}
    check_paths(&paths)?;Ok(paths)
}
pub fn filesystem_selection(view:&IShellView)->Result<bool>{
    let folder:IFolderView2=view.cast()?;
    if unsafe{folder.ItemCount(SVGIO_SELECTION)?}==0{return Ok(true);}
    let items:IShellItemArray=unsafe{folder.Items(SVGIO_SELECTION)?};
    Ok(unsafe{items.GetAttributes(SIATTRIBFLAGS_AND,SFGAO_FILESYSTEM)?}.0&SFGAO_FILESYSTEM.0!=0)
}
pub(super) fn item_path(item:&IShellItem)->Result<PathBuf>{
    let value=unsafe{item.GetDisplayName(SIGDN_FILESYSPATH)?};let text=unsafe{value.to_string()};unsafe{CoTaskMemFree(Some(value.0.cast()));}Ok(text.map(PathBuf::from)?)
}
pub(super) fn paste_folder(view:&IShellView)->Result<Option<IShellItem>>{
    let folder:IFolderView2=view.cast()?;let persistent:IPersistFolder2=unsafe{folder.GetFolder()?};
    let pidl=unsafe{persistent.GetCurFolder()?};
    if pidl.is_null(){return Err(Error::from_hresult(HRESULT(0x80004005u32 as i32)));}
    let desktop=unsafe{ILGetSize(Some(pidl))}==2;
    let result=if desktop{Ok(None)}else{unsafe{SHCreateItemFromIDList::<IShellItem>(pidl)}.map(Some)};
    unsafe{CoTaskMemFree(Some(pidl.cast()));}result
}
pub fn folder_path(view:&IShellView)->Result<PathBuf>{
    if let Some(item)=paste_folder(view)?{item_path(&item)}else{crate::desktop::user_desktop().ok_or_else(||invalid("Desktop folder unavailable"))}
}
pub fn filesystem_folder_path(view:&IShellView)->Result<Option<PathBuf>>{
    let Some(item)=paste_folder(view)? else{return Ok(crate::desktop::user_desktop());};
    let attributes=unsafe{item.GetAttributes(windows::Win32::System::SystemServices::SFGAO_FOLDER|SFGAO_FILESYSTEM|windows::Win32::System::SystemServices::SFGAO_STREAM)?}.0;
    if attributes&(SFGAO_FILESYSTEM.0|windows::Win32::System::SystemServices::SFGAO_FOLDER.0)!=(SFGAO_FILESYSTEM.0|windows::Win32::System::SystemServices::SFGAO_FOLDER.0)
        ||attributes&windows::Win32::System::SystemServices::SFGAO_STREAM.0!=0{return Ok(None);}
    item_path(&item).map(Some)
}
fn move_intent(effect:u32)->bool{effect&7==2}
fn same_cut_selection(a:&[PathBuf],b:&[PathBuf])->bool{
    if a.len()!=b.len(){return false;}
    if a==b{return true;}
    let keys=|paths:&[PathBuf]|{let mut keys:Vec<_>=paths.iter().map(|p|completion_key(p)).collect();keys.sort_unstable();keys};
    keys(a)==keys(b)
}
pub fn copy_view(view:&IShellView,owner:HWND,cut:bool)->Result<()>{
    let sequence = unsafe { GetClipboardSequenceNumber() };
    let paths = selected_paths(view)?;
    if paths.is_empty() { return Ok(()); }
    if !cut { return copy_paths(owner, &paths, false); }

    // Keep the selected view's Shell formats (including its source identity).
    // Publish MOVE before asking DefView to ghost the selection.
    let data = match unsafe { view.GetItemObject(SVGIO_SELECTION) } {
        Ok(data) if set_drop_effect(&data, true).is_ok() => data,
        _ => file_data_object(owner, &paths, true)?,
    };
    let folder = view.cast::<IShellFolderView>();
    let window = unsafe { view.GetWindow() }.ok();
    publish_view_cut(&data, sequence, || Ok(same_cut_selection(&selected_paths(view)?, &paths)), || {
        let marked = folder.is_ok_and(|folder| {
            let status = unsafe { (folder.vtable().SetClipboard)(folder.as_raw(), true.into()) };
            status == windows::Win32::Foundation::S_OK
        });
        if let Some(hwnd) = window {
            if !marked {
                // ShellFolderView_SetClipboard specifies DWORD(-2) for a cut.
                const CUT_TO_CLIPBOARD: u32 = -2i32 as u32;
                unsafe { SHShellFolderView_Message(hwnd, SFVM_SETCLIPBOARD, LPARAM(CUT_TO_CLIPBOARD as isize)); }
            }
            use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE};
            let _ = unsafe { RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_ALLCHILDREN) };
        }
    })
}

// OleIsCurrentClipboard returns S_FALSE for a different object. Preserve that
// distinction: the generated Result<()> binding maps both success codes to Ok.
#[link(name = "ole32")]
unsafe extern "system" {
    #[link_name = "OleIsCurrentClipboard"]
    fn clipboard_object_status(object: *mut core::ffi::c_void) -> HRESULT;
}

fn publish_view_cut(
    data: &windows::Win32::System::Com::IDataObject,
    sequence: u32,
    same_selection: impl Fn() -> Result<bool>,
    mark: impl FnOnce(),
) -> Result<()> {
    if !same_selection()? || unsafe { GetClipboardSequenceNumber() } != sequence {
        return Err(invalid(zsdesk_core::i18n::text("剪贴板已改变，请重新剪切")));
    }
    unsafe { windows::Win32::System::Ole::OleSetClipboard(data)?; }
    if same_selection().unwrap_or(false) && unsafe { clipboard_object_status(data.as_raw()) } == windows::Win32::Foundation::S_OK {
        mark();
    }
    Ok(())
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
/// Snapshot only file paths marked for a move. Other clipboard formats are
/// neither read nor rewritten when refreshing a custom view's cut appearance.
pub fn cut_clipboard_paths()->Result<Vec<PathBuf>>{
    if unsafe{IsClipboardFormatAvailable(15)}.is_err(){return Ok(Vec::new());}
    let (paths,effect,_)=clipboard_files()?;
    Ok(if move_intent(effect){paths}else{Vec::new()})
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
pub(crate) fn parse_hdrop(bytes:&[u8])->Result<Vec<PathBuf>>{
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

#[derive(Clone,Copy,Debug,PartialEq,Eq)]pub enum Operation{Copy,Move,Delete{permanent:bool},NewFolder}
#[derive(Clone,Debug,Default,PartialEq,Eq)]struct Access{read:Vec<Vec<u16>>,write:Vec<Vec<u16>>}
impl Access{
    fn new(paths:&[PathBuf],destination:Option<&Path>,operation:Operation)->Self{
        let mut value=Self::default();
        for path in paths{
            if operation==Operation::Copy{value.read.push(completion_key(path));}else{value.write.push(completion_key(path));}
            if matches!(operation,Operation::Copy|Operation::Move){if let Some(destination)=destination{
                value.write.push(completion_key(&path.file_name().map_or_else(||destination.to_path_buf(),|name|destination.join(name))));
            }}
        }
        if operation==Operation::NewFolder{if let Some(destination)=destination{value.write.push(completion_key(destination));}}
        value.read.sort();value.read.dedup();value.write.sort();value.write.dedup();value
    }
    fn overlaps(a:&[Vec<u16>],b:&[Vec<u16>])->bool{
        let parent=|a:&[u16],b:&[u16]|a==b||b.starts_with(a)&&(a.last()==Some(&92)||b.get(a.len())==Some(&92));
        let(mut i,mut j)=(0,0);while i<a.len()&&j<b.len(){
            if parent(&a[i],&b[j])||parent(&b[j],&a[i]){return true;}
            if a[i]<b[j]{i+=1;}else{j+=1;}
        }false
    }
    fn conflicts(&self,other:&Self)->bool{Self::overlaps(&self.write,&other.write)||Self::overlaps(&self.write,&other.read)||Self::overlaps(&self.read,&other.write)}
}
#[derive(Debug)]struct ReplySignal(usize);
impl Drop for ReplySignal{fn drop(&mut self){let _=unsafe{CloseHandle(HANDLE(self.0 as*mut _))};}}
#[derive(Debug)]enum DropReply{Complete(Completion),SetupError{code:HRESULT,message:String}}
#[derive(Clone,Debug)]struct Reply{sender:mpsc::SyncSender<DropReply>,ready:Arc<ReplySignal>}
#[derive(Clone,Debug)]struct Request{token:usize,owner:isize,thread:u32,origin:Option<TransferOrigin>,paths:Vec<PathBuf>,destination:Option<PathBuf>,operation:Operation,clipboard:Option<u32>,reply:Option<Reply>,access:Access,
    #[cfg(test)]test_gate:Option<Arc<ReplySignal>>,
}
pub fn paste(owner:HWND,destination:&Path)->Result<()>{
    // An opened .zip reports its own file path; only its Shell folder can paste into it.
    if !destination.is_absolute()||!destination.is_dir(){return Err(invalid("目标文件夹无效"));}
    if try_paste_files(owner,destination)?{return Ok(());}
    // Providers may advertise both a path list and a virtual Shell payload.
    // Delegate only before any filesystem job has been submitted.
    let folder=crate::desktop::folder_for_path(destination)?;
    let menu=super::menu::ShellMenu::for_folder_background(owner,&folder)?;
    menu.populate_for_invoke(false)?;
    menu.invoke_verb(owner,"paste",false)
}
pub fn try_paste_files(owner:HWND,destination:&Path)->Result<bool>{
    if !has_filesystem_clipboard() || !destination.is_absolute() || !destination.is_dir(){return Ok(false);}
    let sequence_before=unsafe{GetClipboardSequenceNumber()};
    let (paths,effect,sequence)=match clipboard_files(){
        Ok(packet)=>packet,
        Err(e) if e.code()==HRESULT(0x80070057u32 as i32)&&has_shell_payload()&&unsafe{GetClipboardSequenceNumber()}==sequence_before=>return Ok(false),
        Err(error)=>return Err(error),
    };
    if effect==4{return Ok(false);} // Preferred shell link.
    submit(owner,paths,Some(destination.into()),if move_intent(effect){Operation::Move}else{Operation::Copy},Some(sequence))?;
    Ok(true)
}
pub fn paste_view(view:&IShellView,owner:HWND)->Result<()>{paste(owner,&folder_path(view)?)}
pub fn delete_view(view:&IShellView,owner:HWND,permanent:bool)->Result<()>{
    let paths=selected_paths(view)?;if paths.is_empty(){return Ok(());}delete_paths(owner,paths,permanent)
}
pub fn delete_paths(owner:HWND,paths:Vec<PathBuf>,permanent:bool)->Result<()>{submit(owner,paths,None,Operation::Delete{permanent},None)}
pub fn transfer_paths(owner:HWND,paths:Vec<PathBuf>,destination:PathBuf,moving:bool)->Result<()>{submit(owner,paths,Some(destination),if moving{Operation::Move}else{Operation::Copy},None)}
/// Physical HDROP transfer on the shared scheduler. Dependent paths retain FIFO
/// ordering. IDropTarget::Drop must receive
/// the actual result before reporting an effect to its source. The caller
/// must release application state borrows before entering this message pump.
pub fn transfer_drop(owner:HWND,paths:Vec<PathBuf>,destination:PathBuf,moving:bool)->Result<Completion>{
    if !unsafe{IsWindow(Some(owner))}.as_bool()||!owns_view_thread(owner){return Err(invalid("拖放目标窗口归属无效"));}
    let mut request=prepare_request(owner,paths,Some(destination),if moving{Operation::Move}else{Operation::Copy},None)?;
    let ready=Arc::new(ReplySignal(unsafe{CreateEventW(None,true,false,PCWSTR::null())}?.0 as usize));
    let (sender,receive)=mpsc::sync_channel(1);request.reply=Some(Reply{sender,ready:ready.clone()});
    enqueue(request)?;
    struct Quit(Option<i32>);impl Drop for Quit{fn drop(&mut self){if let Some(code)=self.0{unsafe{PostQuitMessage(code);}}}}
    let mut quit=Quit(None);let handle=HANDLE(ready.0 as*mut _);
    loop{
        match receive.try_recv(){
            Ok(DropReply::Complete(completion))=>return Ok(completion),
            Ok(DropReply::SetupError{code,message})=>return Err(Error::new(code,message)),
            Err(mpsc::TryRecvError::Disconnected)=>return Err(Error::new(HRESULT(0x80004005u32 as i32),"文件操作结果未返回，无法确认拖放完成")),
            Err(mpsc::TryRecvError::Empty)=>{},
        }
        // Dispatch UI input/paint and preserve a pending shutdown. Neither a
        // timeout nor WM_QUIT is mistaken for a completed physical move.
        let mut message=MSG::default();
        for _ in 0..128{
            if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
            if message.message==WM_QUIT{quit.0=Some(message.wParam.0 as i32);continue;}
            unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}
        }
        let flags=(COWAIT_DISPATCH_CALLS.0|COWAIT_INPUTAVAILABLE.0)as u32;
        if let Err(error)=unsafe{CoWaitForMultipleHandles(flags,25,&[handle])}{
            // RPC_S_CALLPENDING is only a short pump interval. A caller without
            // a COM apartment still gets a Win32 pump until the queued job ends.
            if error.code().0 as u32!=0x80010115{
                unsafe{MsgWaitForMultipleObjectsEx(Some(&[handle]),25,QS_ALLINPUT,MWMO_INPUTAVAILABLE);}
            }
        }
    }
}
pub fn new_folder(owner:HWND,destination:&Path)->Result<()>{submit(owner,Vec::new(),Some(destination.into()),Operation::NewFolder,None)}
pub fn take_created_folder(owner:HWND)->Option<PathBuf>{
    let root=unsafe{GetAncestor(owner,GA_ROOT)};let owner=if root.0.is_null(){owner}else{root};
    let thread=unsafe{GetWindowThreadProcessId(owner,None)};
    let mut created=CREATED.lock().unwrap_or_else(|error|error.into_inner());
    let index=created.iter().position(|(window,id,_)|*window==owner.0 as isize&&*id==thread)?;
    created.remove(index).map(|(_,_,path)|path)
}
fn store_completion(request:&Request,completion:Completion){
    let mut completed=COMPLETIONS.lock().unwrap_or_else(|error|error.into_inner());
    completed.retain(|(_,window,thread,_)|unsafe{IsWindow(Some(HWND(*window as*mut _)))}.as_bool()&&unsafe{GetWindowThreadProcessId(HWND(*window as*mut _),None)}==*thread);
    if completed.len()>=64{completed.pop_front();}
    completed.push_back((request.token,request.owner,request.thread,completion));
}
fn completion_for(request:&Request,result:Result<Outcome>)->Completion{
    match result{
        Ok(outcome)=>Completion{operation:request.operation,requested:request.paths.len().max(1),completed:outcome.completed,
            aborted:outcome.aborted,cancelled:outcome.cancelled,skipped:outcome.skipped,outputs:outcome.outputs,
            outputs_truncated:outcome.outputs_truncated,origin:request.origin,error:outcome.error,error_detail:None},
        Err(error)=>Completion{operation:request.operation,requested:request.paths.len().max(1),completed:0,aborted:true,
            cancelled:is_cancelled(error.code()),skipped:false,outputs:Vec::new(),outputs_truncated:false,origin:request.origin,
            error:Some(error.code()),error_detail:Some(error_message(&error).chars().take(2048).collect())},
    }
}
fn prepare_request(owner:HWND,paths:Vec<PathBuf>,destination:Option<PathBuf>,operation:Operation,clipboard:Option<u32>)->Result<Request>{
    if !matches!(operation,Operation::NewFolder){check_paths(&paths)?;}
    if let Some(destination)=&destination{check_paths(&[destination.clone()])?;}
    let origin=capture_origin(owner);
    let root=unsafe{GetAncestor(owner,GA_ROOT)};let owner=if root.0.is_null(){owner}else{root};
    let access=Access::new(&paths,destination.as_deref(),operation);
    Ok(Request{token:NEXT_OPERATION.fetch_add(1,Ordering::Relaxed),owner:owner.0 as isize,thread:unsafe{GetWindowThreadProcessId(owner,None)},origin,paths,destination,operation,clipboard,reply:None,access,
        #[cfg(test)]test_gate:None})
}
fn submit(owner:HWND,paths:Vec<PathBuf>,destination:Option<PathBuf>,operation:Operation,clipboard:Option<u32>)->Result<()>{
    enqueue(prepare_request(owner,paths,destination,operation,clipboard)?)
}
fn enqueue(request:Request)->Result<()>{
    let message=result_message();pending_operations();
    let mut workers=WORKERS.lock().unwrap_or_else(|error|error.into_inner());
    let mut queue=QUEUE.lock().unwrap_or_else(|error|error.into_inner());
    if request.clipboard.is_some()&&request.reply.is_none()&&queue.waiting.iter().chain(queue.active.iter()).any(|other|
        other.clipboard==request.clipboard&&other.operation==request.operation&&other.access==request.access&&other.destination==request.destination){return Ok(());}
    if queue.waiting.len()+queue.active.len()>=16{return Err(invalid("文件操作队列已满，请等待当前操作完成"));}
    let token=request.token;queue.waiting.push_back(request);
    let desired=(queue.waiting.len()+queue.active.len()).min(MAX_WORKERS);
    while queue.workers<desired{
    queue.workers+=1;
    let worker=thread::Builder::new().name("zsdesk-file-operation".into()).spawn(move||{
        struct Worker(bool);impl Drop for Worker{fn drop(&mut self){if self.0{let mut queue=QUEUE.lock().unwrap_or_else(|error|error.into_inner());queue.workers=queue.workers.saturating_sub(1);}}}let mut worker_count=Worker(true);
        loop{
            let request={let mut queue=QUEUE.lock().unwrap_or_else(|error|error.into_inner());
                let index=(0..queue.waiting.len()).find(|index|{
                    let access=&queue.waiting[*index].access;
                    !queue.active.iter().any(|active|access.conflicts(&active.access))
                        &&!queue.waiting.iter().take(*index).any(|earlier|access.conflicts(&earlier.access))
                });
                let Some(index)=index else{queue.workers-=1;worker_count.0=false;break;};let request=queue.waiting.remove(index).unwrap();queue.active.push(request.clone());request};
            struct Active(usize);impl Drop for Active{fn drop(&mut self){QUEUE.lock().unwrap_or_else(|error|error.into_inner()).active.retain(|request|request.token!=self.0);}}let _active=Active(request.token);
            let result=std::panic::catch_unwind(||{
                #[cfg(test)]if let Some(gate)=&request.test_gate{
                    let waited=unsafe{windows::Win32::System::Threading::WaitForSingleObject(HANDLE(gate.0 as*mut _),5000)};
                    if waited!=windows::Win32::Foundation::WAIT_OBJECT_0{return Err(Error::new(HRESULT(0x800705b4u32 as i32),"test UI pump did not release the FIFO gate"));}
                }
                perform(&request,false)
            }).unwrap_or_else(|_|Err(Error::from_hresult(HRESULT(0x80004005u32 as i32))));
            if let Some(reply)=&request.reply{
                let answer=match result{Ok(outcome)=>DropReply::Complete(completion_for(&request,Ok(outcome))),
                    Err(error)=>DropReply::SetupError{code:error.code(),message:error.message().chars().take(2048).collect()}};
                // The shared Arc keeps the event valid even if the caller
                // receives the channel result before this SetEvent executes.
                let _=reply.sender.try_send(answer);let _=unsafe{SetEvent(HANDLE(reply.ready.0 as*mut _))};continue;
            }
            let owner=HWND(request.owner as*mut _);
            if unsafe{IsWindow(Some(owner))}.as_bool()&&unsafe{GetWindowThreadProcessId(owner,None)}==request.thread{
                // The compatibility queue only carries fully completed NewFolder
                // operations. Partial outputs live in their exact completion.
                if let Ok(outcome)=&result{if !outcome.aborted{if let Some(path)=&outcome.created{
                    let mut created=CREATED.lock().unwrap_or_else(|error|error.into_inner());
                    created.retain(|(window,thread,_)|unsafe{IsWindow(Some(HWND(*window as*mut _)))}.as_bool()&&unsafe{GetWindowThreadProcessId(HWND(*window as*mut _),None)}==*thread);
                    if created.len()>=32{created.pop_front();}created.push_back((request.owner,request.thread,path.clone()));
                }}}
                let completion=completion_for(&request,result);let code=completion.result_code();store_completion(&request,completion);
                let _=unsafe{PostMessageW(Some(owner),message,WPARAM(code as usize),LPARAM(request.token as isize))};
            }
        }
    });
    let worker=match worker{Ok(worker)=>worker,Err(error)=>{queue.workers=queue.workers.saturating_sub(1);queue.waiting.retain(|request|request.token!=token);return Err(Error::new(HRESULT(0x80004005u32 as i32),error.to_string()));}};
    workers.push(worker);
    }Ok(())
}
fn completion_key(path:&Path)->Vec<u16>{
    use std::os::windows::ffi::OsStrExt;
    // Preserve every non-ASCII UTF-16 unit. This is deliberately narrower than
    // filesystem-wide Unicode case folding; uncertain matches remain pending.
    let mut units:Vec<u16>=path.as_os_str().encode_wide().map(|unit|match unit{
        65..=90=>unit+32,47=>92,_=>unit,
    }).collect();
    if units.get(..8)==Some(&[92,92,63,92,117,110,99,92]){units.drain(..6);units[0]=92;units[1]=92;}
    else if units.get(..4)==Some(&[92,92,63,92])&&units.get(4).is_some_and(|unit|(97..=122).contains(unit))&&units.get(5)==Some(&58){units.drain(..4);}
    while units.len()>3&&units.last()==Some(&92){units.pop();}
    units
}
fn exact_completion_key(path:&Path)->Vec<u16>{
    use std::os::windows::ffi::OsStrExt;
    let mut units:Vec<_>=path.as_os_str().encode_wide().map(|value|if value==47{92}else{value}).collect();
    if units.get(..4)==Some(&[92,92,63,92]){
        let unc=units.get(4..8).is_some_and(|value|value.iter().map(|unit|if (65..=90).contains(unit){unit+32}else{*unit}).eq([117,110,99,92]));
        if unc{units.drain(..6);units[0]=92;units[1]=92;}
        else if units.get(5)==Some(&58){units.drain(..4);}
    }
    if units.get(1)==Some(&58)&&units.first().is_some_and(|unit|(65..=90).contains(unit)){units[0]+=32;}
    while units.len()>3&&units.last()==Some(&92){units.pop();}units
}
#[derive(Default)]struct ProgressState{
    finished:Option<HRESULT>,failure:Option<HRESULT>,skipped:bool,cancelled:bool,
    expected:HashSet<Vec<u16>>,completed:HashSet<Vec<u16>>,created:Option<PathBuf>,
    aliases:HashMap<Vec<u16>,Option<Vec<u16>>>,new_folder:bool,new_completed:bool,track_outputs:bool,
    outputs:Vec<PathBuf>,output_keys:HashSet<Vec<u16>>,output_bytes:usize,outputs_truncated:bool,
}
impl ProgressState{
    fn fail(&mut self,error:HRESULT){
        self.cancelled|=is_cancelled(error);
        if self.failure.is_none()||self.failure.is_some_and(is_cancelled)&&!is_cancelled(error){self.failure=Some(error);}
    }
    fn new(operation:Operation)->Self{
        Self{new_folder:operation==Operation::NewFolder,
            track_outputs:matches!(operation,Operation::Copy|Operation::Move|Operation::NewFolder),..Default::default()}
    }
    fn expect_source(&mut self,requested:&Path,shell_path:&Path){
        // Bind before the operation: a moved or deleted source may no longer
        // exist when Shell reports its long filesystem name to the callback.
        let exact=exact_completion_key(shell_path);
        self.expected.insert(exact.clone());
        let mut alias=|path:&Path|{
            self.aliases.entry(completion_key(path)).and_modify(|entry|{
                if entry.as_ref()!=Some(&exact){*entry=None;}
            }).or_insert_with(||Some(exact.clone()));
        };
        alias(shell_path);
        if requested!=shell_path{alias(requested);}
    }
    #[cfg(test)]fn for_request(request:&Request)->Self{
        let mut state=Self::new(request.operation);
        for path in &request.paths{state.expect_source(path,path);}
        state
    }
    fn record_completed(&mut self,path:&Path)->bool{
        let exact=exact_completion_key(path);
        let key=if self.expected.contains(&exact){Some(exact)}else{self.aliases.get(&completion_key(path)).cloned().flatten()};
        if let Some(key)=key{self.completed.insert(key);true}else{false}
    }
    fn is_top_level(&self,source:Option<&Path>,new:bool)->bool{
        if new{return self.new_folder;}
        source.is_some_and(|path|self.expected.contains(&exact_completion_key(path))||self.aliases.get(&completion_key(path)).is_some_and(Option::is_some))
    }
    fn output(&mut self,path:&Path){
        if !self.track_outputs{return;}
        let units=wide(path);if !path.is_absolute()||units.len()>32768||units[..units.len()-1].contains(&0){self.outputs_truncated=true;return;}
        let key=exact_completion_key(path);if self.output_keys.contains(&key){return;}
        let Some(bytes)=self.output_bytes.checked_add(units.len()*2)else{self.outputs_truncated=true;return;};
        if self.outputs.len()>=MAX_ITEMS||bytes>MAX_PATH_BYTES{self.outputs_truncated=true;return;}
        self.output_keys.insert(key);self.outputs.push(path.into());self.output_bytes=bytes;
    }
    fn success(&mut self,source:Option<&Path>,output:Option<&Path>,new:bool){
        let top=if new{self.new_folder}else{source.is_some_and(|path|self.record_completed(path))};
        if !top{return;}
        if new{self.new_completed=true;if let Some(output)=output{self.created=Some(output.into());}}
        if self.track_outputs{if let Some(path)=output{self.output(path);}else{self.outputs_truncated=true;}}
    }
}
#[implement(IFileOperationProgressSink)]struct Progress{state:Arc<Mutex<ProgressState>>,cancel_before:bool}
#[cfg(test)]thread_local!{static FAIL_AFTER_SUCCESS:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
impl Progress{
    fn before(&self)->Result<()>{
        #[cfg(test)]if FAIL_AFTER_SUCCESS.with(std::cell::Cell::get){
            let mut state=self.state.lock().unwrap_or_else(|error|error.into_inner());
            if !state.completed.is_empty(){let code=HRESULT(0x80070005u32 as i32);state.fail(code);return Err(Error::from_hresult(code));}
        }
        if self.cancel_before{Err(Error::from_hresult(COPYENGINE_E_CANCELLED))}else{Ok(())}
    }
    fn after(&self,item:windows::core::Ref<IShellItem>,result:HRESULT,created:windows::core::Ref<IShellItem>,new:bool)->Result<()>{
        if result.is_err(){self.state.lock().unwrap_or_else(|error|error.into_inner()).fail(result);return Ok(());}
        if [COPYENGINE_S_PENDING,COPYENGINE_S_PENDING_DELETE].contains(&result){return Ok(());}
        if [COPYENGINE_S_USER_IGNORED,COPYENGINE_S_NOT_HANDLED].contains(&result)||result==HRESULT(1){self.state.lock().unwrap_or_else(|error|error.into_inner()).skipped=true;return Ok(());}
        // Only requested top-level sources contribute output paths. A recursive
        // directory copy must not claim every descendant or unrelated new file.
        // Bookkeeping errors never abort an otherwise successful Shell operation.
        let source=item.as_ref().and_then(|item|item_path(item).ok());
        let track_outputs={let state=self.state.lock().unwrap_or_else(|error|error.into_inner());
            if !state.is_top_level(source.as_deref(),new){return Ok(());}state.track_outputs};
        // Never hold our mutex across a provider COM call or resolve every
        // recursive descendant's destination just to discard it afterwards.
        let output=if track_outputs{created.as_ref().and_then(|item|item_path(item).ok())}else{None};
        self.state.lock().unwrap_or_else(|error|error.into_inner()).success(source.as_deref(),output.as_deref(),new);
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
#[derive(Debug)]struct Outcome{
    aborted:bool,created:Option<PathBuf>,completed:usize,cancelled:bool,skipped:bool,
    outputs:Vec<PathBuf>,outputs_truncated:bool,error:Option<HRESULT>,
}
fn finish_outcome(state:&mut ProgressState,performed:Result<()>,native_aborted:Result<bool>)->Outcome{
    let perform_error=performed.err().map(|error|error.code());
    let abort_error=native_aborted.as_ref().err().map(|error|error.code());
    let finished_error=state.finished.filter(|result|result.is_err());
    // A final cancellation may follow an earlier real per-item failure. Keep
    // that failure while retaining every positively confirmed successful item.
    let errors=[state.failure,perform_error,finished_error,abort_error];
    let failure=errors.iter().flatten().copied().find(|code|!is_cancelled(*code))
        .or_else(||errors.iter().flatten().copied().next());
    let cancelled=state.cancelled||errors.iter().flatten().any(|code|is_cancelled(*code))
        ||(native_aborted.as_ref().is_ok_and(|value|*value)&&!state.skipped&&failure.is_none());
    let mut aborted=native_aborted.unwrap_or(true)||failure.is_some()||state.skipped||state.finished.is_none();
    if state.new_folder{aborted|=!state.new_completed;}else{aborted|=state.completed.len()!=state.expected.len();}
    Outcome{aborted,created:state.created.clone(),completed:if state.new_folder{usize::from(state.new_completed)}else{state.completed.len()},
        cancelled,skipped:state.skipped,outputs:std::mem::take(&mut state.outputs),outputs_truncated:state.outputs_truncated,error:failure}
}
fn operation_flags(operation:Operation,quiet:bool)->FILEOPERATION_FLAGS{
    let mut flags=FOF_NOCONFIRMMKDIR|FOF_ALLOWUNDO|FOFX_ADDUNDORECORD;
    match operation{Operation::Delete{permanent:false}=>flags|=FOFX_RECYCLEONDELETE|FOF_WANTNUKEWARNING,
        Operation::Delete{permanent:true}=>{flags=FOF_WANTNUKEWARNING;},_=>{}}
    if quiet{flags|=FOF_SILENT|FOF_NOERRORUI|FOFX_EARLYFAILURE|FOF_NOCONFIRMATION;}
    flags
}
struct NativeProgress(IOperationsProgressDialog);
impl Drop for NativeProgress{fn drop(&mut self){let _=unsafe{self.0.StopProgressDialog()};}}
fn perform(request:&Request,quiet:bool)->Result<Outcome>{perform_inner(request,quiet,false)}
fn perform_inner(request:&Request,quiet:bool,cancel_before:bool)->Result<Outcome>{
    if !matches!(request.operation,Operation::NewFolder){check_paths(&request.paths)?;}
    unsafe{OleInitialize(None)?;}struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let operation:IFileOperation=unsafe{CoCreateInstance(&FileOperation,None,CLSCTX_INPROC_SERVER)?};
    let mut flags=operation_flags(request.operation,quiet);
    // Copying within the same directory creates a new name, as Explorer does.
    // Ordinary cross-directory collisions retain the system replace prompt.
    if matches!(request.operation,Operation::Copy)&&request.destination.as_ref().is_some_and(|destination|
        request.paths.iter().all(|path|path.parent().is_some_and(|parent|parent==destination||
            std::fs::canonicalize(parent).ok().zip(std::fs::canonicalize(destination).ok()).is_some_and(|(a,b)|a==b)))){flags|=FOF_RENAMEONCOLLISION;}
    unsafe{operation.SetOperationFlags(flags)?;}
    let mut state=ProgressState::new(request.operation);
    let mut identities=Vec::with_capacity(request.paths.len());
    for path in &request.paths{
        let item:IShellItem=unsafe{SHCreateItemFromParsingName(PCWSTR(wide(path).as_ptr()),None)?};
        let shell_path=item_path(&item).ok();
        state.expect_source(path,shell_path.as_deref().unwrap_or(path));
        identities.push(super::pidl::item_pidl(&item).ok_or_else(||invalid("无法创建文件标识"))?);
    }
    let progress=Arc::new(Mutex::new(state));
    let sink:IFileOperationProgressSink=Progress{state:progress.clone(),cancel_before}.into();
    let cookie=unsafe{operation.Advise(&sink)}?;
    struct Advisory<'a>{operation:&'a IFileOperation,cookie:u32}impl Drop for Advisory<'_>{fn drop(&mut self){let _=unsafe{self.operation.Unadvise(self.cookie)};}}let _advisory=Advisory{operation:&operation,cookie};
    let owner=HWND(request.owner as*mut _);
    if unsafe{IsWindow(Some(owner))}.as_bool()&&unsafe{GetWindowThreadProcessId(owner,None)}==request.thread{unsafe{operation.SetOwnerWindow(owner)?;}}
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
    // Explicitly own the native progress UI across the worker operation. File
    // collisions remain Shell-owned and never inherit silent/Yes-to-All flags.
    let _dialog=if !quiet&&matches!(request.operation,Operation::Copy|Operation::Move){
        let dialog:IOperationsProgressDialog=unsafe{CoCreateInstance(&CLSID_ProgressDialog,None,CLSCTX_INPROC_SERVER)}?;
        // IFileOperation owns Start/SetOperation/Stop ordering; calling
        // SetOperation before Start returns E_UNEXPECTED on Windows.
        unsafe{operation.SetProgressDialog(&dialog)?;}
        Some(NativeProgress(dialog))
    }else{None};
    let performed=unsafe{operation.PerformOperations()};
    let native_aborted=unsafe{operation.GetAnyOperationsAborted()}.map(|value|value.as_bool());
    let mut state=progress.lock().unwrap_or_else(|error|error.into_inner());
    let outcome=finish_outcome(&mut state,performed,native_aborted);
    drop(state);
    if !outcome.aborted&&request.operation==Operation::Move{if let Some(sequence)=request.clipboard{
        if let Ok(_clipboard)=Clipboard::open(None){if unsafe{GetClipboardSequenceNumber()}==sequence{let _=unsafe{EmptyClipboard()};}}
    }}
    Ok(outcome)
}

#[cfg(test)]#[path="file_transfer_tests.rs"]mod tests;
