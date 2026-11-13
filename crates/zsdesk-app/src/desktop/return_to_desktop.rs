//! Move out of a fence using Shell file operations, then publish loose desktop
//! icons. Files already on either desktop only change visual membership.
use std::{cell::RefCell, path::{Path, PathBuf}, rc::Rc, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::Duration};
use windows::{core::{implement, Error, HRESULT, Interface, PCWSTR, Ref, Result}, Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::{Com::{CoCreateInstance, CoTaskMemFree, IDataObject, CLSCTX_INPROC_SERVER},
        Ole::{OleInitialize, OleUninitialize, DROPEFFECT, DROPEFFECT_MOVE, DROPEFFECT_NONE}},
    UI::{Shell::{FileOperation, IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl,
        IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH, FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FOFX_ADDUNDORECORD},
        WindowsAndMessaging::*},
}};
use zsdesk_windows::{desktop::{self, ShellEntry}, ole::{DropHandler, DropRegistration}};
use super::{host, with_host};

static ACTIVE: AtomicBool = AtomicBool::new(false);
pub(super) fn pending() -> usize { usize::from(ACTIVE.load(Ordering::Acquire)) }
fn error(message: &str) -> Error { Error::new(HRESULT(0x80004005u32 as i32), message) }
fn wide(path: &Path) -> Vec<u16> { use std::os::windows::ffi::OsStrExt; path.as_os_str().encode_wide().chain(Some(0)).collect() }
fn same_path(a: &Path, b: &Path) -> bool { crate::explorer_forwarder::same_path(a, b) }
fn completed_move(result:HRESULT)->bool {
    // Same-volume renames can complete the whole item with the success code
    // COPYENGINE_S_DONT_PROCESS_CHILDREN instead of S_OK (sherrors.h).
    matches!(result.0 as u32,0|0x00270008|0x0027000a)
}
fn already_on_desktop(entry: &ShellEntry, user: &Path, public: Option<&Path>) -> bool {
    entry.path.as_ref().is_none_or(|path| path.parent().is_some_and(|parent| same_path(parent, user) || public.is_some_and(|public| same_path(parent, public))))
}

#[derive(Default)]
struct ProgressState { completed: Vec<String>, failed: Option<HRESULT> }
#[implement(IFileOperationProgressSink)]
struct Progress { state: Arc<Mutex<ProgressState>>, cancel_before: bool }
#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for Progress_Impl {
    fn StartOperations(&self)->Result<()> { Ok(()) }
    fn FinishOperations(&self,result:HRESULT)->Result<()> { if result.is_err() { self.state.lock().unwrap_or_else(|e|e.into_inner()).failed=Some(result); } Ok(()) }
    fn PreRenameItem(&self,_:u32,_:Ref<IShellItem>,_:&PCWSTR)->Result<()> { Ok(()) }
    fn PostRenameItem(&self,_:u32,_:Ref<IShellItem>,_:&PCWSTR,_:HRESULT,_:Ref<IShellItem>)->Result<()> { Ok(()) }
    fn PreMoveItem(&self,_:u32,_:Ref<IShellItem>,_:Ref<IShellItem>,_:&PCWSTR)->Result<()> {
        if self.cancel_before { Err(Error::from_hresult(HRESULT(0x800704c7u32 as i32))) } else { Ok(()) }
    }
    fn PostMoveItem(&self,_:u32,_:Ref<IShellItem>,_:Ref<IShellItem>,_:&PCWSTR,result:HRESULT,created:Ref<IShellItem>)->Result<()> {
        #[cfg(test)] eprintln!("PostMoveItem result=0x{:08X} destination={}",result.0 as u32,created.as_ref().is_some());
        // Only completed move statuses with an actual destination object count;
        // skipped, pending, collision prompts and cancellation never invent names.
        if completed_move(result) {
            if let Some(item)=created.as_ref() {
                if let Ok(value)=unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) } {
                    let path=unsafe { value.to_string() }; unsafe { CoTaskMemFree(Some(value.0.cast())) };
                    if let Ok(path)=path { self.state.lock().unwrap_or_else(|e|e.into_inner()).completed.push(path); }
                }
            }
        } else if result.is_err() { self.state.lock().unwrap_or_else(|e|e.into_inner()).failed=Some(result); }
        Ok(())
    }
    fn PreCopyItem(&self,_:u32,_:Ref<IShellItem>,_:Ref<IShellItem>,_:&PCWSTR)->Result<()> { Ok(()) }
    fn PostCopyItem(&self,_:u32,_:Ref<IShellItem>,_:Ref<IShellItem>,_:&PCWSTR,_:HRESULT,_:Ref<IShellItem>)->Result<()> { Ok(()) }
    fn PreDeleteItem(&self,_:u32,_:Ref<IShellItem>)->Result<()> { Ok(()) }
    fn PostDeleteItem(&self,_:u32,_:Ref<IShellItem>,_:HRESULT,_:Ref<IShellItem>)->Result<()> { Ok(()) }
    fn PreNewItem(&self,_:u32,_:Ref<IShellItem>,_:&PCWSTR)->Result<()> { Ok(()) }
    fn PostNewItem(&self,_:u32,_:Ref<IShellItem>,_:&PCWSTR,_:&PCWSTR,_:u32,_:HRESULT,_:Ref<IShellItem>)->Result<()> { Ok(()) }
    fn UpdateProgress(&self,_:u32,_:u32)->Result<()> { Ok(()) }
    fn ResetTimer(&self)->Result<()> { Ok(()) } fn PauseTimer(&self)->Result<()> { Ok(()) } fn ResumeTimer(&self)->Result<()> { Ok(()) }
}

fn move_files(owner: isize, paths: Vec<PathBuf>, destination: PathBuf, progress: Arc<Mutex<ProgressState>>) -> Result<()> {
    move_files_inner(owner,paths,destination,progress,false,false)
}
fn move_files_inner(owner: isize, paths: Vec<PathBuf>, destination: PathBuf, progress: Arc<Mutex<ProgressState>>, quiet:bool, cancel_before:bool) -> Result<()> {
    unsafe { OleInitialize(None)?; }
    struct Apartment; impl Drop for Apartment { fn drop(&mut self) { unsafe { OleUninitialize(); } } }
    let _apartment=Apartment;
    let operation:IFileOperation=unsafe { CoCreateInstance(&FileOperation,None,CLSCTX_INPROC_SERVER)? };
    let target:IShellItem=unsafe { SHCreateItemFromParsingName(PCWSTR(wide(&destination).as_ptr()),None)? };
    let sink:IFileOperationProgressSink=Progress { state:progress,cancel_before }.into();
    unsafe {
        let flags=if quiet { use windows::Win32::UI::Shell::*; FOF_SILENT|FOF_NOERRORUI|FOFX_EARLYFAILURE|FOF_NOCONFIRMATION }
            else { FOF_ALLOWUNDO|FOFX_ADDUNDORECORD|FOF_NOCONFIRMMKDIR };
        operation.SetOperationFlags(flags)?;
        if IsWindow(Some(HWND(owner as *mut _))).as_bool() { operation.SetOwnerWindow(HWND(owner as *mut _))?; }
        for path in paths {
            let item:IShellItem=SHCreateItemFromParsingName(PCWSTR(wide(&path).as_ptr()),None)?;
            operation.MoveItem(&item,&target,PCWSTR::null(),&sink)?;
        }
        operation.PerformOperations()?;
        if operation.GetAnyOperationsAborted()?.as_bool() {
            return Err(Error::from_hresult(HRESULT(0x800704c7u32 as i32)));
        }
    }
    Ok(())
}

/// The worker owns all filesystem/COM work. The caller pumps the desktop STA
/// while the native progress dialog is active, outside every HOST borrow.
fn transfer(owner:HWND,paths:Vec<PathBuf>,target:PathBuf)->(Vec<String>,Option<Error>) {
    let progress=Arc::new(Mutex::new(ProgressState::default()));
    let work=progress.clone(); let owner=owner.0 as isize;
    let worker=std::thread::Builder::new().name("zsdesk-return-desktop".into()).spawn(move||move_files(owner,paths,target,work));
    let Ok(worker)=worker else { return (Vec::new(),Some(error("无法启动桌面文件移动"))); };
    let mut quit=None;
    while !worker.is_finished() {
        let mut message=MSG::default();
        for _ in 0..64 {
            if !unsafe { PeekMessageW(&mut message,None,0,0,PM_REMOVE) }.as_bool() { break; }
            if message.message==WM_QUIT { quit=Some(message.wParam.0 as i32); continue; }
            unsafe { let _=TranslateMessage(&message); DispatchMessageW(&message); }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result=worker.join().unwrap_or_else(|_|Err(error("桌面文件移动未能正常结束")));
    if let Some(code)=quit { unsafe { PostQuitMessage(code) }; }
    let mut state=progress.lock().unwrap_or_else(|e|e.into_inner());
    (std::mem::take(&mut state.completed),result.err().or_else(||state.failed.map(Error::from_hresult)))
}

pub(super) fn return_entries(owner:HWND,entries:&[ShellEntry],point:POINT)->bool {
    if entries.is_empty() || entries.len()>10_000 || ACTIVE.swap(true,Ordering::AcqRel) { return false; }
    struct Active; impl Drop for Active { fn drop(&mut self) { ACTIVE.store(false,Ordering::Release); } }
    let _active=Active;
    let Some(target)=desktop::user_desktop() else { return false; };
    let public=desktop::public_desktop();
    let mut keys=Vec::new(); let mut paths=Vec::new();let mut unique=std::collections::HashSet::new();
    for entry in entries {
        if !unique.insert(entry.key.to_lowercase()){continue;}
        if already_on_desktop(entry,&target,public.as_deref()) { keys.push(entry.key.clone()); }
        else if let Some(path)=&entry.path { paths.push(path.clone()); }
    }
    let requested=unique.len();
    let mut failure=None;
    if !paths.is_empty() { let (moved,error)=transfer(owner,paths,target); keys.extend(moved); failure=error; }
    let published=if keys.is_empty(){0}else{
        match with_host(|host|host.release_desktop_keys(&keys,point)) {
            Some(Ok(count))=>count,
            Some(Err(message))=>{failure=Some(error(&message));0},
            None=>{failure=Some(error("桌面窗口未能完成分组调整；已移动的文件仍保留在桌面目录。"));0},
        }
    };
    if published!=requested && failure.is_none() {
        failure=Some(error("部分项目未能移回桌面，未完成的文件仍保留在原位置。"));
    }
    let complete=published==requested && failure.is_none();
    if let Some(error)=failure { super::file_action_error(owner,&error); }
    complete
}

pub(super) fn return_selection(fence:u64,point:POINT) {
    let Some((owner,entries))=with_host(|host|host.win_index(fence).map(|_|
        (host.hwnd,host.selected_entries_owned(fence)))).flatten() else { return; };
    return_entries(owner,&entries,point);
    super::refresh_fence(fence);
}

/// Only present during an owned drag. It sits directly above the real desktop,
/// below fences/apps; no permanent full-screen input surface is installed.
pub(super) struct DesktopDropSurfaces { windows:Vec<(HWND,Option<DropRegistration>)>, completed:Rc<RefCell<bool>> }
impl Drop for DesktopDropSurfaces {
    fn drop(&mut self) { for (window,registration) in &mut self.windows { *registration=None; let _=unsafe { DestroyWindow(*window) }; } }
}
struct ReturnDrop { owner:HWND, source:u64, expected:IDataObject, entries:Vec<ShellEntry>, completed:Rc<RefCell<bool>>, active:CellState }
type CellState=std::cell::Cell<bool>;
impl ReturnDrop {
    fn effect(&self,keys:u32,allowed:DROPEFFECT)->DROPEFFECT {
        // Ctrl/Alt explicitly request copy/link, so let those drops be rejected
        // here rather than silently moving the user's files.
        if !self.active.get() || keys & (8|32)!=0 || allowed.0&DROPEFFECT_MOVE.0==0 { return DROPEFFECT_NONE; }
        if with_host(|host|host.internal_drag.as_ref().is_some_and(|(source,_)|*source==self.source)).unwrap_or(false) { DROPEFFECT_MOVE } else { DROPEFFECT_NONE }
    }
}
impl DropHandler for ReturnDrop {
    fn enter(&self,data:&IDataObject,keys:u32,_:POINT,allowed:DROPEFFECT)->DROPEFFECT { self.active.set(data==&self.expected); self.effect(keys,allowed) }
    fn over(&self,keys:u32,_:POINT,allowed:DROPEFFECT)->DROPEFFECT { self.effect(keys,allowed) }
    fn leave(&self) { self.active.set(false); }
    fn dropped(&self,data:&IDataObject,keys:u32,point:POINT,allowed:DROPEFFECT)->DROPEFFECT {
        if data!=&self.expected || self.effect(keys,allowed)==DROPEFFECT_NONE { return DROPEFFECT_NONE; }
        let complete=return_entries(self.owner,&self.entries,point);
        *self.completed.borrow_mut()=complete;
        if complete { DROPEFFECT_MOVE } else { DROPEFFECT_NONE }
    }
}
unsafe extern "system" fn surface_proc(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT {
    if msg==WM_MOUSEACTIVATE { return LRESULT(MA_NOACTIVATE as isize); }
    unsafe { DefWindowProcW(hwnd,msg,wp,lp) }
}
impl DesktopDropSurfaces {
    pub(super) fn new(owner:HWND,source:u64,data:&IDataObject,entries:Vec<ShellEntry>)->Self {
        let completed=Rc::new(RefCell::new(false));
        let mut surfaces=Self { windows:Vec::new(),completed:completed.clone() };
        let enabled=with_host(|host|host.config.settings.organize).unwrap_or(false);
        if !enabled { return surfaces; }
        let Some(desktop)=desktop::desktop_defview().map(|view|unsafe { GetAncestor(view,GA_ROOT) }).filter(|root|!root.0.is_null()) else { return surfaces; };
        let class=windows::core::w!("ZSDesk.DesktopReturnDrop");
        let instance=unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }.ok().map(|value|windows::Win32::Foundation::HINSTANCE(value.0));
        unsafe { RegisterClassW(&WNDCLASSW { lpfnWndProc:Some(surface_proc),hInstance:instance.unwrap_or_default(),lpszClassName:class,..Default::default() }); }
        for monitor in host::monitors() {
            let rect=monitor.work;
            let Ok(window)=(unsafe { CreateWindowExW(WS_EX_LAYERED|WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,class,PCWSTR::null(),WS_POPUP,
                rect.left,rect.top,rect.right-rect.left,rect.bottom-rect.top,Some(desktop),None,instance,None) }) else { continue; };
            let handler=Rc::new(ReturnDrop { owner,source,expected:data.clone(),entries:entries.clone(),completed:completed.clone(),active:CellState::new(false) });
            let registration=unsafe { DropRegistration::register(window,handler) }.ok();
            if registration.is_none() { let _=unsafe { DestroyWindow(window) }; continue; }
            let _=unsafe { SetLayeredWindowAttributes(window,Default::default(),1,LWA_ALPHA) };
            let above=unsafe { GetWindow(desktop,GW_HWNDPREV) }.unwrap_or(HWND_TOP);
            let _=unsafe { SetWindowPos(window,Some(above),rect.left,rect.top,rect.right-rect.left,rect.bottom-rect.top,SWP_NOACTIVATE|SWP_SHOWWINDOW) };
            surfaces.windows.push((window,registration));
        }
        surfaces
    }
    pub(super) fn completed(&self)->bool { *self.completed.borrow() }
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn desktop_parent_comparison_does_not_treat_descendants_as_desktop_items() {
        let user=Path::new(r"C:\Users\Test\Desktop");
        assert!(same_path(user,Path::new(r"c:\users\test\Desktop")));
        assert!(!same_path(user,Path::new(r"C:\Users\Test\Desktop\Folder")));
    }
    #[test] fn move_success_does_not_confuse_skipped_or_pending_results() {
        for result in [0,0x00270008,0x0027000a]{assert!(completed_move(HRESULT(result)));}
        for result in [1u32,0x00270003,0x00270004,0x00270005,0x00270006,0x0027000b,0x0027000e,0x800704c7]{assert!(!completed_move(HRESULT(result as i32)));}
    }

    #[test]
    #[ignore="Native IFileOperation fixture only; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_desktop_return_moves_actual_destination_and_preserves_cancelled_source()->anyhow::Result<()> {
        use anyhow::{Context,ensure};
        use std::{fs,io::Write};
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("output")?).join("desktop-return");
        ensure!(root.is_absolute(),"absolute fixture output required");
        let source=root.join("source");let target=root.join("target");fs::create_dir_all(&source)?;fs::create_dir_all(&target)?;
        let first=source.join("移回 桌面.txt");let cancelled=source.join("保留.txt");let untouched=target.join("已有文件.txt");
        for (path,bytes) in [(&first,b"first-content".as_slice()),(&cancelled,b"cancel-preserves-content".as_slice()),(&untouched,b"unrelated".as_slice())] {
            fs::OpenOptions::new().write(true).create_new(true).open(path)?.write_all(bytes)?;
        }
        let progress=Arc::new(Mutex::new(ProgressState::default()));
        move_files_inner(0,vec![first.clone()],target.clone(),progress.clone(),true,false)?;
        let actual=progress.lock().unwrap().completed.clone();ensure!(actual.len()==1,"missing actual completed destination");
        ensure!(same_path(Path::new(&actual[0]),&target.join("移回 桌面.txt")),"wrong completion target");
        ensure!(!first.try_exists()?&&fs::read(&actual[0])?==b"first-content","move lost contents");
        let cancelled_progress=Arc::new(Mutex::new(ProgressState::default()));
        let _=move_files_inner(0,vec![cancelled.clone()],target.clone(),cancelled_progress.clone(),true,true);
        ensure!(cancelled_progress.lock().unwrap().completed.is_empty(),"cancel reported completion");
        ensure!(fs::read(&cancelled)?==b"cancel-preserves-content"&&!target.join("保留.txt").try_exists()?,"cancel changed source");
        ensure!(fs::read(&untouched)?==b"unrelated","unrelated file changed");
        fs::remove_file(&actual[0])?;fs::remove_file(&cancelled)?;fs::remove_file(&untouched)?;
        println!("{{\"state\":\"passed\",\"real_desktop_modified\":false,\"undo_history_modified\":false,\"cancel_preserved_source\":true}}");
        Ok(())
    }
}
